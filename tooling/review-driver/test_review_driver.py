"""Contract tests for the client-fed review driver."""

from __future__ import annotations

import subprocess
import sys
import unittest
import uuid
from pathlib import Path
from unittest.mock import patch


sys.path.insert(0, str(Path(__file__).parent))

from review_driver import (  # noqa: E402
    CONCERNS,
    CompletedPass,
    DriverFailure,
    PassIdentities,
    PullRequestFacts,
    ReviewDriver,
    TranscriptSnapshot,
    attempt_identities,
    frozen_configuration_digest,
    parser,
    reserved_template_names,
    run_process,
    template_catalog_versions,
    transcript_snapshot,
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

    def template_versions(self, names):
        return {name: "1" for name in names}

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

    def transcript(
        self, session_id: str, turn_id: str | None = None
    ) -> TranscriptSnapshot:
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

    def template_versions(self, names):
        return {name: "1" for name in names}
    def commission(self, *args, **kwargs) -> str:
        raise AssertionError("a complete attempt must not commission a session")

    def transcript(
        self, session_id: str, turn_id: str | None = None
    ) -> TranscriptSnapshot:
        raise AssertionError("a complete attempt must not read a transcript")


class TerminalRecheckSessions:
    def __init__(self) -> None:
        self.read_index = 0
        self.turn_id = str(uuid.UUID(int=3))

    def template_versions(self, names):
        return {name: "1" for name in names}

    def commission(
        self,
        facts: PullRequestFacts,
        template: str,
        command_id: str,
        statement: str,
        content: str,
    ) -> str:
        return str(uuid.UUID(int=1))

    def transcript(
        self, session_id: str, turn_id: str | None = None
    ) -> TranscriptSnapshot:
        states = (
            "active_running",
            "active_running",
            "completed",
            "active_running",
        )
        state = states[self.read_index]
        self.read_index += 1
        frontier = str(uuid.UUID(int=4)) if state == "completed" else None
        return TranscriptSnapshot(
            session_id=session_id,
            accepted_input_id=str(uuid.UUID(int=2)),
            turn_id=self.turn_id,
            turn_state=state,
            terminal_frontier_id=frontier,
            assistant_text="imported context",
        )


class ConcernFanoutCli(FakeReviewCli):
    def __init__(self) -> None:
        super().__init__(state="awaiting_concerns")
        self.recorded_concerns: list[tuple[str, str]] = []

    def record_import_outcome(
        self,
        attempt_id: str,
        completed: CompletedPass,
        outcome: str,
        context_digest: str | None,
    ) -> None:
        raise AssertionError("the fan-out fixture starts after import")

    def record_concern_outcome(
        self, attempt_id: str, concern: str, completed: CompletedPass, outcome: str
    ) -> None:
        self.recorded_concerns.append((concern, outcome))


class ConcernFanoutSessions:
    """Terminalizes one chosen member unsuccessfully and the rest normally."""

    def __init__(self, failing_index: int = 0) -> None:
        self.commissioned: list[str] = []
        self.failing_index = failing_index

    def template_versions(self, names):
        return {name: "1" for name in names}

    def commission(
        self,
        facts: PullRequestFacts,
        template: str,
        command_id: str,
        statement: str,
        content: str,
    ) -> str:
        session = str(uuid.uuid5(uuid.NAMESPACE_URL, command_id))
        if session not in self.commissioned:
            self.commissioned.append(session)
        return session

    def transcript(
        self, session_id: str, turn_id: str | None = None
    ) -> TranscriptSnapshot:
        failing = self.commissioned.index(session_id) == self.failing_index
        return TranscriptSnapshot(
            session_id=session_id,
            accepted_input_id=str(uuid.UUID(int=2)),
            turn_id=str(uuid.UUID(int=3)),
            turn_state="failed" if failing else "completed",
            terminal_frontier_id=str(uuid.UUID(int=4)),
            assistant_text="concern output",
        )


class RecordingImportCli(FakeReviewCli):
    def __init__(self) -> None:
        super().__init__(state="awaiting_import")
        self.pass_outcomes: list[str] = []
        self.import_outcomes: list[tuple[str, str | None]] = []

    def complete_pass(self, completed: CompletedPass, outcome: str) -> None:
        super().complete_pass(completed, outcome)
        self.pass_outcomes.append(outcome)

    def record_import_outcome(
        self,
        attempt_id: str,
        completed: CompletedPass,
        outcome: str,
        context_digest: str | None,
    ) -> None:
        # Mirrors the daemon's `validate_import`: a non-succeeded import claim
        # is only compatible with a pass that was itself sealed with that
        # same outcome (`ReviewPassState::Failed`/`Blocked`/`Cancelled`).
        # Sealing the pass as `succeeded` and then reporting a `failed`
        # import is exactly the incompatible combination the daemon rejects.
        if outcome != "succeeded" and self.pass_outcomes[-1:] != [outcome]:
            raise AssertionError(
                "the daemon rejects a non-succeeded import outcome whose pass "
                f"was not sealed with a matching outcome "
                f"(pass_outcomes={self.pass_outcomes!r}, import outcome={outcome!r})"
            )
        self.import_outcomes.append((outcome, context_digest))


class ContextlessImportSessions:
    """A turn that reaches `completed` without producing imported context."""

    def template_versions(self, names):
        return {name: "1" for name in names}

    def commission(
        self,
        facts: PullRequestFacts,
        template: str,
        command_id: str,
        statement: str,
        content: str,
    ) -> str:
        return str(uuid.UUID(int=1))

    def transcript(
        self, session_id: str, turn_id: str | None = None
    ) -> TranscriptSnapshot:
        return TranscriptSnapshot(
            session_id=session_id,
            accepted_input_id=str(uuid.UUID(int=2)),
            turn_id=str(uuid.UUID(int=3)),
            turn_state="completed",
            terminal_frontier_id=str(uuid.UUID(int=4)),
            assistant_text="   \n  ",
        )


class ReviewDriverTests(unittest.TestCase):
    def test_arguments_reject_invalid_repository_and_pull_request(self) -> None:
        with self.assertRaises(SystemExit):
            parser().parse_args(["missing-owner", "41", "/tmp/signalbox.sock"])
        with self.assertRaises(SystemExit):
            parser().parse_args([REPOSITORY, "0", "/tmp/signalbox.sock"])
        with self.assertRaises(SystemExit):
            parser().parse_args(["sample-owner/.", "41", "/tmp/signalbox.sock"])
        with self.assertRaises(SystemExit):
            parser().parse_args(["sample-owner/..", "41", "/tmp/signalbox.sock"])

    def test_external_command_timeout_is_typed(self) -> None:
        timeout = subprocess.TimeoutExpired(["signalbox"], 0.25)

        with patch("review_driver.subprocess.run", side_effect=timeout):
            with self.assertRaises(DriverFailure) as caught:
                run_process(["signalbox"], "start-run", 0.25)

        self.assertEqual(caught.exception.code, "stage-timeout")
        self.assertEqual(caught.exception.stage, "start-run")

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

        expected = attempt_identities(
            facts,
            frozen_configuration_digest(
                sessions.template_versions(reserved_template_names())
            ),
        )
        self.assertTrue(cli.activation_interrupted)
        self.assertEqual(resumed, expected)
        self.assertEqual(len(cli.targets), 1)
        self.assertEqual(len(cli.attempts), 1)
        self.assertEqual(len(cli.started_runs), 1)
        self.assertEqual(len(sessions.sessions_by_command), 1)
        self.assertEqual(len(cli.completed_passes), 1)

    def test_transcript_pins_first_turn_when_a_later_turn_is_terminal(self) -> None:
        first_turn = str(uuid.UUID(int=3))
        second_turn = str(uuid.UUID(int=5))
        first_input = str(uuid.UUID(int=2))
        messages = [
            {
                "type": "transcript_turn",
                "turn_id": first_turn,
                "acceptance_position": "1",
                "state": {"type": "active_running"},
            },
            {
                "type": "transcript_turn",
                "turn_id": second_turn,
                "acceptance_position": "2",
                "state": {
                    "type": "reconciliation_required",
                    "terminal_frontier_id": str(uuid.UUID(int=8)),
                },
            },
            {
                "type": "transcript_user_entry",
                "entry_index": "1",
                "accepted_input_id": first_input,
                "turn_id": first_turn,
                "content": [],
            },
            {
                "type": "transcript_text_entry",
                "entry_index": "2",
                "entry": {"type": "assistant", "turn_id": first_turn},
            },
            {
                "type": "transcript_content",
                "entry_index": "2",
                "content_fragment": "first output",
            },
            {
                "type": "transcript_user_entry",
                "entry_index": "3",
                "accepted_input_id": str(uuid.UUID(int=4)),
                "turn_id": second_turn,
                "content": [],
            },
            {
                "type": "transcript_text_entry",
                "entry_index": "4",
                "entry": {"type": "assistant", "turn_id": second_turn},
            },
            {
                "type": "transcript_content",
                "entry_index": "4",
                "content_fragment": "second output",
            },
        ]

        snapshot = transcript_snapshot(messages, str(uuid.UUID(int=1)))

        self.assertEqual(snapshot.accepted_input_id, first_input)
        self.assertEqual(snapshot.turn_id, first_turn)
        self.assertEqual(snapshot.turn_state, "active_running")
        self.assertEqual(snapshot.assistant_text, "first output")

    def test_transcript_recovers_accepted_input_after_the_turn_leaves_queued(
        self,
    ) -> None:
        turn = str(uuid.UUID(int=3))
        accepted_input = str(uuid.UUID(int=2))
        # Only `queued` carries `accepted_input_id` on the turn projection, so a
        # turn that has already started exposes that identity exclusively
        # through its native `transcript_user_entry` member.
        messages = [
            {
                "type": "transcript_turn",
                "turn_id": turn,
                "acceptance_position": "1",
                "state": {"type": "active_running"},
            },
            {
                "type": "transcript_user_entry",
                "entry_index": "1",
                "accepted_input_id": accepted_input,
                "turn_id": turn,
                "content": [],
            },
        ]

        snapshot = transcript_snapshot(messages, str(uuid.UUID(int=1)))

        self.assertEqual(snapshot.accepted_input_id, accepted_input)
        self.assertEqual(snapshot.turn_id, turn)
        self.assertEqual(snapshot.turn_state, "active_running")

    def test_duration_arguments_reject_non_finite_values(self) -> None:
        for option in ("--timeout-seconds", "--poll-seconds"):
            for value in ("nan", "inf", "-inf", "0"):
                with self.subTest(option=option, value=value):
                    with self.assertRaises(SystemExit):
                        parser().parse_args(
                            [
                                REPOSITORY,
                                str(PULL_REQUEST),
                                "/tmp/signalbox.sock",
                                option,
                                value,
                            ]
                        )

    def test_complete_pass_rechecks_exact_turn_terminality(self) -> None:
        facts = pull_request_facts(HEAD_ONE)
        cli = FakeReviewCli(state="awaiting_import")
        sessions = TerminalRecheckSessions()
        driver = ReviewDriver(FakeGitHub(facts), cli, sessions, 1.0, 0.001)

        with self.assertRaises(DriverFailure) as caught:
            driver.run(REPOSITORY, PULL_REQUEST)

        self.assertEqual(caught.exception.code, "terminal-evidence-invalid")
        self.assertEqual(len(cli.completed_passes), 0)

    def test_fanout_commissions_every_concern_despite_an_early_failure(self) -> None:
        facts = pull_request_facts(HEAD_ONE)
        cli = ConcernFanoutCli()
        sessions = ConcernFanoutSessions(failing_index=0)
        driver = ReviewDriver(FakeGitHub(facts), cli, sessions, 1.0, 0.001)

        with self.assertRaises(DriverFailure) as caught:
            driver.run(REPOSITORY, PULL_REQUEST)

        # Every durable concern slot is still commissioned before any terminal
        # outcome is collected; aborting the loop on the first unsuccessful
        # member would strand the remaining durable slots forever.
        self.assertEqual(len(sessions.commissioned), len(CONCERNS))
        # Only the unsuccessful member carries a recorded claim.  A `succeeded`
        # concern claim is rebuilt by the daemon from the sealed pass and the
        # fan-out barrier admits it only when that pass carries a
        # `ProducedFindings` inventory, which no generic completion can supply.
        self.assertEqual(cli.recorded_concerns, [(CONCERNS[0][0], "failed")])
        self.assertEqual(caught.exception.code, "stage-terminal-unsuccessful")

    def test_successful_concerns_stop_before_generic_completion(self) -> None:
        facts = pull_request_facts(HEAD_ONE)
        cli = ConcernFanoutCli()
        # No member fails: every commissioned concern turn reaches `completed`.
        sessions = ConcernFanoutSessions(failing_index=-1)
        driver = ReviewDriver(FakeGitHub(facts), cli, sessions, 1.0, 0.001)

        with self.assertRaises(DriverFailure) as caught:
            driver.run(REPOSITORY, PULL_REQUEST)

        # `handle_complete_review_pass` refuses `--outcome succeeded` for a
        # read-only-review pass, whose sole success admission is the typed
        # findings inventory, and a `succeeded` concern claim is rebuilt from
        # that sealed pass.  Sending either seal here would classify every
        # successful concern as a command error and make the declared typed
        # boundary unreachable, so the fan-out stops short of both.
        self.assertEqual(len(sessions.commissioned), len(CONCERNS))
        self.assertEqual(cli.completed_passes, set())
        self.assertEqual(cli.recorded_concerns, [])
        self.assertEqual(caught.exception.code, "typed-stage-output-unavailable")
        self.assertEqual(caught.exception.stage, "concerns")

    def test_import_without_context_is_not_a_successful_operation(self) -> None:
        facts = pull_request_facts(HEAD_ONE)
        cli = RecordingImportCli()
        driver = ReviewDriver(
            FakeGitHub(facts), cli, ContextlessImportSessions(), 1.0, 0.001
        )

        with self.assertRaises(DriverFailure) as caught:
            driver.run(REPOSITORY, PULL_REQUEST)

        # The terminal turn still authenticates the pass, but the workflow
        # operation must not advance the attempt on the turn lifecycle alone.
        # The pass must be sealed with the same outcome ultimately reported
        # to record-import-outcome, or the daemon's validate_import rejects
        # the claim as an incompatible pass/import-outcome combination.
        self.assertEqual(cli.pass_outcomes, ["failed"])
        self.assertEqual(cli.import_outcomes, [("failed", None)])
        self.assertEqual(caught.exception.code, "stage-terminal-unsuccessful")
        self.assertEqual(caught.exception.stage, "import")

    def test_changed_frozen_configuration_creates_a_new_attempt(self) -> None:
        facts = pull_request_facts(HEAD_ONE)
        catalog = {name: "1" for name in reserved_template_names()}
        before = attempt_identities(facts, frozen_configuration_digest(catalog))
        moved_template = attempt_identities(
            facts,
            frozen_configuration_digest({**catalog, "review-import": "2"}),
        )
        with patch(
            "review_driver.CONCERNS",
            CONCERNS + (("performance", "review-concern-performance"),),
        ):
            widened_set = attempt_identities(
                facts, frozen_configuration_digest(catalog)
            )
        with patch("review_driver.CONCERN_SET_VERSION", "initial-six-v1"):
            bumped_version = attempt_identities(
                facts, frozen_configuration_digest(catalog)
            )

        # The target is the immutable snapshot and must not fork per
        # configuration; the attempt and every command it owns must.
        for changed in (moved_template, widened_set, bumped_version):
            self.assertEqual(before.target, changed.target)
            self.assertNotEqual(before.attempt, changed.attempt)

    def test_template_catalog_projects_only_the_reserved_templates(self) -> None:
        messages = [
            {"type": "templates_start"},
            {"type": "template_summary", "name": "review-import", "version": "3"},
            {"type": "template_summary", "name": "unrelated-template", "version": "9"},
            {"type": "templates_end", "template_count": "2"},
        ]

        projected = template_catalog_versions(
            messages, reserved_template_names()
        )

        self.assertEqual(projected, {"review-import": "3"})

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

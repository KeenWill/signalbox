#!/usr/bin/env python3
"""Unit tests for the standalone convergence reconciler."""

from __future__ import annotations

import math
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

from reconcile import (
    Config,
    GitHubGraphQL,
    GitHubNotFoundError,
    PaginationTask,
    PULL_REQUEST_DETAILS_QUERY,
    choose_decision,
    completed_codex_review_summary,
    comment_only_patch,
    configured_path,
    empty_page_info,
    evaluate_convergence,
    is_codex_review_request,
    load_state,
    normalize_pull_request,
    normalize_review_threads,
    nonnegative_number,
    pagination_query,
    positive_number,
    prior_threads_dispositioned_before,
    process_pull_request,
    review_comment_signature,
    review_signature,
    review_thread_signature,
    save_state,
)


class ConvergencePredicateTests(unittest.TestCase):
    def test_description_requirements_block_convergence(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "body": " ".join(["word"] * 351),
            "checked_head_oid": "head-description",
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "head_oid": "head-description",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["head-description"],
            "review_threads": [],
        }

        computed = evaluate_convergence(pull_request)

        self.assertFalse(computed["converged"])
        self.assertEqual(
            computed["reasons"],
            ["description-exceeds-350-words"],
        )

    def test_review_exempt_head_change_preserves_quiet_review(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-exempt",
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "head_oid": "head-exempt",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": [],
            "review_exempt_since_quiet_review": True,
            "review_threads": [],
        }

        computed = evaluate_convergence(pull_request)

        self.assertTrue(computed["converged"])
        self.assertEqual(computed["reasons"], [])

    def test_green_checks_resolved_threads_and_mergeable_base_converge(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-green",
            "check_rollup_state": "SUCCESS",
            "head_oid": "head-green",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["head-green"],
            "review_threads": [
                {"isResolved": True, "isDispositioned": True}
            ],
            "checks": [
                {
                    "__typename": "CheckRun",
                    "name": "required build",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                },
                {
                    "__typename": "CheckRun",
                    "name": "Tool live smokes (report only)",
                    "status": "COMPLETED",
                    "conclusion": "FAILURE",
                },
            ],
        }

        computed = evaluate_convergence(pull_request)

        self.assertTrue(computed["converged"])
        self.assertEqual(computed["reasons"], [])
        self.assertEqual(
            computed["gating_checks"],
            [{"name": "required build", "state": "SUCCESS"}],
        )
        self.assertEqual(
            computed["non_gating_checks"],
            [
                {"name": "Tool live smokes (report only)", "state": "FAILURE"},
            ],
        )

    def test_coverage_context_is_gating_without_report_only_label(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-coverage",
            "check_rollup_state": "FAILURE",
            "checks": [
                {
                    "__typename": "StatusContext",
                    "context": "codecov/patch",
                    "state": "FAILURE",
                }
            ],
            "head_oid": "head-coverage",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["head-coverage"],
            "review_threads": [],
        }

        computed = evaluate_convergence(pull_request)

        self.assertFalse(computed["converged"])
        self.assertEqual(
            computed["reasons"],
            ["check-not-green:codecov/patch:FAILURE"],
        )

    def test_non_tool_context_cannot_claim_report_only_exemption(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-impostor",
            "check_rollup_state": "FAILURE",
            "checks": [
                {
                    "__typename": "CheckRun",
                    "name": "OpenAI smoke compatibility (report only)",
                    "status": "COMPLETED",
                    "conclusion": "FAILURE",
                }
            ],
            "head_oid": "head-impostor",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["head-impostor"],
            "review_threads": [],
        }

        computed = evaluate_convergence(pull_request)

        self.assertFalse(computed["converged"])
        self.assertEqual(
            computed["reasons"],
            [
                "check-not-green:OpenAI smoke compatibility (report only):FAILURE"
            ],
        )

    def test_completed_codex_summary_requires_current_head(self) -> None:
        comment = {
            "author": {"login": "chatgpt-codex-connector"},
            "body": (
                "<!-- codex-pull-request-review-summary -->\n"
                "| 📝 **Code Review** | ✅ **Completed** "
                '<relative-time datetime="2026-09-05T18:35:26.344586Z">now'
                "</relative-time> | `abcdef1` | User request |"
            ),
            "createdAt": "2026-09-05T17:02:17Z",
            "lastEditedAt": "2026-09-05T18:35:26Z",
        }

        self.assertEqual(
            completed_codex_review_summary(comment, "abcdef1234567890"),
            ("abcdef1234567890", "2026-09-05T18:35:26.344586Z"),
        )
        self.assertIsNone(
            completed_codex_review_summary(comment, "fffffff234567890")
        )

    def test_required_provider_smoke_failure_blocks_convergence(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-smoke",
            "check_rollup_state": "FAILURE",
            "head_oid": "head-smoke",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["head-smoke"],
            "review_threads": [],
            "checks": [
                {
                    "__typename": "CheckRun",
                    "name": "OpenAI smoke compatibility",
                    "status": "COMPLETED",
                    "conclusion": "FAILURE",
                }
            ],
        }

        computed = evaluate_convergence(pull_request)

        self.assertFalse(computed["converged"])
        self.assertEqual(
            computed["reasons"],
            ["check-not-green:OpenAI smoke compatibility:FAILURE"],
        )

    def test_incomplete_review_thread_census_fails_closed(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "_review_thread_nodes": [{"id": "thread-1"}],
            "_thread_total_count": 2,
            "_thread_page": {"hasNextPage": False, "endCursor": None},
            "_check_page": {"hasNextPage": False, "endCursor": None},
            "_file_page": {"hasNextPage": False, "endCursor": None},
        }

        with self.assertRaisesRegex(RuntimeError, "incomplete census"):
            client._finish_paginated_connections([pull_request])

    def test_review_thread_revalidation_detects_reopened_early_page(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)

        def thread(index: int, resolved: bool) -> dict[str, object]:
            return {
                "id": f"thread-{index}",
                "isResolved": resolved,
                "comments": {
                    "totalCount": 0,
                    "nodes": [],
                    "pageInfo": empty_page_info(),
                },
            }

        first_page = [
            thread(index, index != 0)
            for index in range(100)
        ]
        client.execute = mock.Mock(
            side_effect=[
                {
                    "node": {
                        "baseRefOid": "base",
                        "headRefOid": "head",
                        "reviewThreads": {
                            "totalCount": 101,
                            "nodes": first_page,
                            "pageInfo": {
                                "hasNextPage": True,
                                "endCursor": "cursor",
                            },
                        },
                        "comments": {
                            "nodes": [],
                            "pageInfo": empty_page_info(),
                        },
                        "reviews": {
                            "nodes": [],
                            "pageInfo": empty_page_info(),
                        },
                    }
                },
                {
                    "item0": {
                        "reviewThreads": {
                            "totalCount": 101,
                            "nodes": [thread(100, True)],
                            "pageInfo": empty_page_info(),
                        },
                    }
                },
            ]
        )
        pull_request = {
            "node_id": "pull-request",
            "base_oid": "base",
            "head_oid": "head",
            "checked_head_oid": "head",
            "base_commits_not_in_head": 0,
            "author_login": None,
            "review_threads": [
                {"id": f"thread-{index}", "isResolved": True}
                for index in range(101)
            ],
            "_review_thread_evidence": review_thread_signature(
                normalize_review_threads(
                    [thread(index, True) for index in range(101)], None
                )
            ),
            "_review_comment_evidence": review_comment_signature([]),
            "_review_evidence": review_signature([]),
        }

        client._revalidate_review_threads([pull_request])

        self.assertIsNone(pull_request["checked_head_oid"])
        self.assertIsNone(pull_request["base_commits_not_in_head"])

    def test_review_revalidation_detects_edited_disposition(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        reviewer_comment = {
            "author": {"login": "reviewer"},
            "authorAssociation": "NONE",
            "body": "This code races.",
            "createdAt": "2026-09-05T10:00:00Z",
            "lastEditedAt": None,
            "pullRequestReview": {"id": "review-1"},
        }
        fixed_reply = {
            "author": {"login": "owner"},
            "authorAssociation": "OWNER",
            "body": "Fixed in commit abcdef123.",
            "createdAt": "2026-09-05T10:05:00Z",
            "lastEditedAt": None,
            "pullRequestReview": None,
        }
        edited_reply = {**fixed_reply, "body": "ack"}

        def raw_thread(reply: dict[str, object]) -> dict[str, object]:
            return {
                "id": "thread-1",
                "isResolved": True,
                "comments": {
                    "totalCount": 2,
                    "nodes": [reviewer_comment, reply],
                    "pageInfo": empty_page_info(),
                },
            }

        client.execute = mock.Mock(
            return_value={
                "node": {
                    "baseRefOid": "base",
                    "headRefOid": "head",
                    "reviewThreads": {
                        "totalCount": 1,
                        "nodes": [raw_thread(edited_reply)],
                        "pageInfo": empty_page_info(),
                    },
                    "comments": {
                        "nodes": [],
                        "pageInfo": empty_page_info(),
                    },
                    "reviews": {
                        "nodes": [],
                        "pageInfo": empty_page_info(),
                    },
                }
            }
        )
        pull_request = {
            "node_id": "pull-request",
            "base_oid": "base",
            "head_oid": "head",
            "checked_head_oid": "head",
            "base_commits_not_in_head": 0,
            "author_login": "owner",
            "review_threads": normalize_review_threads(
                [raw_thread(fixed_reply)], "owner"
            ),
            "_review_thread_evidence": review_thread_signature(
                normalize_review_threads([raw_thread(fixed_reply)], "owner")
            ),
            "_review_comment_evidence": review_comment_signature([]),
            "_review_evidence": review_signature([]),
        }

        client._revalidate_review_threads([pull_request])

        self.assertIsNone(pull_request["checked_head_oid"])
        self.assertIsNone(pull_request["base_commits_not_in_head"])

    def test_reviewer_edit_after_disposition_invalidates_thread(self) -> None:
        reviewer_comment = {
            "author": {"login": "reviewer"},
            "authorAssociation": "NONE",
            "body": "This code has a different defect now.",
            "createdAt": "2026-09-05T10:00:00Z",
            "lastEditedAt": "2026-09-05T10:10:00Z",
            "pullRequestReview": {"id": "review-1"},
        }
        prior_reply = {
            "author": {"login": "owner"},
            "authorAssociation": "OWNER",
            "body": "Fixed in commit abcdef123.",
            "createdAt": "2026-09-05T10:05:00Z",
            "lastEditedAt": None,
            "pullRequestReview": None,
        }
        raw_thread = {
            "id": "thread-1",
            "isResolved": True,
            "comments": {
                "totalCount": 2,
                "nodes": [reviewer_comment, prior_reply],
                "pageInfo": empty_page_info(),
            },
        }

        normalized = normalize_review_threads([raw_thread], "owner")[0]

        self.assertEqual(
            normalized["latestReviewerAt"], "2026-09-05T10:10:00Z"
        )
        self.assertFalse(normalized["isDispositioned"])
        self.assertIsNone(normalized["dispositionKind"])

    def test_check_revalidation_detects_an_early_context_change(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)

        def census(conclusion: str) -> dict[str, object]:
            return {
                "node": {
                    "baseRefOid": "base",
                    "headRefOid": "head",
                    "commits": {
                        "nodes": [
                            {
                                "commit": {
                                    "oid": "head",
                                    "statusCheckRollup": {
                                        "state": conclusion,
                                        "contexts": {
                                            "nodes": [
                                                {
                                                    "__typename": "CheckRun",
                                                    "name": "required",
                                                    "status": "COMPLETED",
                                                    "conclusion": conclusion,
                                                    "completedAt": "2026-09-05T00:00:00Z",
                                                }
                                            ],
                                            "pageInfo": empty_page_info(),
                                        },
                                    },
                                }
                            }
                        ]
                    },
                }
            }

        client.execute = mock.Mock(
            side_effect=[census("SUCCESS"), census("FAILURE")]
        )
        pull_request = {
            "node_id": "pull-request",
            "base_oid": "base",
            "head_oid": "head",
            "checked_head_oid": "head",
            "base_commits_not_in_head": 0,
        }

        client._revalidate_checks([pull_request])

        self.assertIsNone(pull_request["checked_head_oid"])
        self.assertIsNone(pull_request["base_commits_not_in_head"])

    def test_per_pr_decision_revalidation_rejects_new_check_identity(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "checks": [
                {
                    "__typename": "CheckRun",
                    "name": "required",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                }
            ],
            "checked_head_oid": "head",
            "base_commits_not_in_head": 0,
        }
        calls: list[str] = []

        def refresh_checks(_pull_requests: object) -> None:
            calls.append("checks")
            pull_request["checks"].append(
                {
                    "__typename": "CheckRun",
                    "name": "late required",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                }
            )

        client._revalidate_checks = mock.Mock(side_effect=refresh_checks)
        client._revalidate_review_threads = mock.Mock(
            side_effect=lambda _pull_requests: calls.append("reviews")
        )
        client._verify_snapshot_oids = mock.Mock(
            side_effect=lambda _pull_requests, **_kwargs: calls.append("oids")
        )

        client.revalidate_for_decision(pull_request)

        self.assertEqual(calls, ["checks", "reviews", "oids"])
        self.assertIsNone(pull_request["checked_head_oid"])
        self.assertIsNone(pull_request["base_commits_not_in_head"])

    def test_unresolved_review_thread_blocks_convergence(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-thread",
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "head_oid": "head-thread",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["head-thread"],
            "review_threads": [
                {"isResolved": False, "isDispositioned": True}
            ],
        }

        computed = evaluate_convergence(pull_request)

        self.assertFalse(computed["converged"])
        self.assertEqual(computed["reasons"], ["unresolved-review-threads:1"])

    def test_pending_gating_check_blocks_convergence(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-pending",
            "check_rollup_state": "PENDING",
            "head_oid": "head-pending",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["head-pending"],
            "review_threads": [],
            "checks": [
                {
                    "__typename": "CheckRun",
                    "name": "required test",
                    "status": "IN_PROGRESS",
                    "conclusion": None,
                }
            ],
        }

        computed = evaluate_convergence(pull_request)

        self.assertFalse(computed["converged"])
        self.assertEqual(
            computed["reasons"],
            ["check-not-green:required test:IN_PROGRESS"],
        )

    def test_present_empty_check_rollup_is_green(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-checks",
            "check_rollup_state": "PENDING",
            "checks": [],
            "head_oid": "head-checks",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["head-checks"],
            "review_threads": [],
        }

        computed = evaluate_convergence(pull_request)

        self.assertTrue(computed["converged"])
        self.assertEqual(computed["reasons"], [])

    def test_check_snapshot_for_an_older_head_blocks_convergence(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "older-head",
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "head_oid": "current-head",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["current-head"],
            "review_threads": [],
        }

        computed = evaluate_convergence(pull_request)

        self.assertFalse(computed["converged"])
        self.assertEqual(computed["reasons"], ["checks-not-for-current-head"])

    def test_base_conflict_blocks_convergence(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-conflict",
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "head_oid": "head-conflict",
            "mergeable": "CONFLICTING",
            "quiet_review_head_oids": ["head-conflict"],
            "review_threads": [],
        }

        computed = evaluate_convergence(pull_request)

        self.assertFalse(computed["converged"])
        self.assertEqual(computed["reasons"], ["base-conflict"])

    def test_unknown_mergeability_blocks_convergence(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-unknown",
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "head_oid": "head-unknown",
            "mergeable": "UNKNOWN",
            "quiet_review_head_oids": ["head-unknown"],
            "review_threads": [],
        }

        computed = evaluate_convergence(pull_request)

        self.assertFalse(computed["converged"])
        self.assertEqual(computed["reasons"], ["mergeability-unknown"])

    def test_missing_quiet_review_for_current_head_blocks_convergence(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "current-head",
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "head_oid": "current-head",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["older-head"],
            "review_threads": [],
        }

        computed = evaluate_convergence(pull_request)

        self.assertFalse(computed["converged"])
        self.assertEqual(
            computed["reasons"],
            ["quiet-review-not-completed-for-current-head"],
        )

    def test_resolved_thread_without_author_reply_blocks_convergence(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-thread",
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "head_oid": "head-thread",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["head-thread"],
            "review_threads": [
                {"isResolved": True, "isDispositioned": False}
            ],
        }

        computed = evaluate_convergence(pull_request)

        self.assertFalse(computed["converged"])
        self.assertEqual(
            computed["reasons"],
            ["undispositioned-review-threads:1"],
        )

    def test_missing_check_rollup_blocks_convergence(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-checks",
            "check_rollup_state": None,
            "checks": [],
            "head_oid": "head-checks",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["head-checks"],
            "review_threads": [],
        }

        computed = evaluate_convergence(pull_request)

        self.assertFalse(computed["converged"])
        self.assertEqual(computed["reasons"], ["check-rollup-missing"])

    def test_current_base_commit_missing_from_head_blocks_convergence(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 2,
            "checked_head_oid": "head-base",
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "head_oid": "head-base",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["head-base"],
            "review_threads": [],
        }

        computed = evaluate_convergence(pull_request)

        self.assertFalse(computed["converged"])
        self.assertEqual(
            computed["reasons"],
            ["base-commits-not-in-head:2"],
        )

    def test_planning_only_pull_request_needs_no_review_wave(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-planning",
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "head_oid": "head-planning",
            "mergeable": "MERGEABLE",
            "planning_only": True,
            "quiet_review_head_oids": [],
            "review_threads": [],
        }

        computed = evaluate_convergence(pull_request)

        self.assertTrue(computed["converged"])
        self.assertEqual(computed["reasons"], [])

    def test_draft_pull_request_blocks_convergence(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-draft",
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "head_oid": "head-draft",
            "is_draft": True,
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["head-draft"],
            "review_threads": [],
        }

        computed = evaluate_convergence(pull_request)

        self.assertEqual(computed["reasons"], ["pull-request-is-draft"])

    def test_changes_requested_review_blocks_convergence(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-review",
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "head_oid": "head-review",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["head-review"],
            "review_decision": "CHANGES_REQUESTED",
            "review_threads": [],
        }

        computed = evaluate_convergence(pull_request)

        self.assertEqual(computed["reasons"], ["review-changes-requested"])

    def test_unsettled_check_inventory_blocks_convergence(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-checks",
            "check_inventory_stable": False,
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "head_oid": "head-checks",
            "mergeable": "MERGEABLE",
            "quiet_review_head_oids": ["head-checks"],
            "review_threads": [],
        }

        computed = evaluate_convergence(pull_request)

        self.assertEqual(computed["reasons"], ["check-inventory-unsettled"])

    def test_open_escalation_marker_does_not_block_convergence(self) -> None:
        pull_request = {
            "base_commits_not_in_head": 0,
            "checked_head_oid": "head-escalated",
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "head_oid": "head-escalated",
            "mergeable": "MERGEABLE",
            "planning_only": False,
            "quiet_review_head_oids": ["head-escalated"],
            "review_threads": [
                {
                    "isResolved": False,
                    "isDispositioned": True,
                    "isEscalated": True,
                }
            ],
        }

        computed = evaluate_convergence(pull_request)

        self.assertTrue(computed["converged"])
        self.assertEqual(computed["escalated_review_threads"], 1)


class DecisionTests(unittest.TestCase):
    def test_converged_pull_request_is_merge_ready(self) -> None:
        decision = choose_decision(
            converged=True,
            now=1000,
            last_dispatched_at=None,
            cool_off_seconds=300,
            active_work=None,
            dry_run=False,
            dispatch_configured=True,
        )

        self.assertEqual(decision.name, "merge-ready")
        self.assertEqual(decision.reason, "convergence-predicate-satisfied")

    def test_recent_dispatch_is_in_cool_off(self) -> None:
        decision = choose_decision(
            converged=False,
            now=1000,
            last_dispatched_at=900,
            cool_off_seconds=300,
            active_work=None,
            dry_run=False,
            dispatch_configured=True,
        )

        self.assertEqual(decision.name, "cooling-off")
        self.assertEqual(decision.reason, "dispatch-cool-off:200s-remaining")

    def test_active_work_prevents_dispatch(self) -> None:
        decision = choose_decision(
            converged=False,
            now=1000,
            last_dispatched_at=None,
            cool_off_seconds=300,
            active_work=True,
            dry_run=False,
            dispatch_configured=True,
        )

        self.assertEqual(decision.name, "already-active")
        self.assertEqual(decision.reason, "operator-command-reported-active-work")

    def test_dry_run_reports_the_dispatch_it_would_make(self) -> None:
        decision = choose_decision(
            converged=False,
            now=1000,
            last_dispatched_at=None,
            cool_off_seconds=300,
            active_work=False,
            dry_run=True,
            dispatch_configured=False,
        )

        self.assertEqual(decision.name, "would-dispatch")
        self.assertEqual(decision.reason, "dry-run-and-no-active-work")

    def test_inactive_work_dispatches_outside_cool_off(self) -> None:
        decision = choose_decision(
            converged=False,
            now=1000,
            last_dispatched_at=600,
            cool_off_seconds=300,
            active_work=False,
            dry_run=False,
            dispatch_configured=True,
        )

        self.assertEqual(decision.name, "dispatch")
        self.assertEqual(decision.reason, "no-active-work-and-outside-cool-off")

    def test_missing_dispatch_command_skips_mutation(self) -> None:
        decision = choose_decision(
            converged=False,
            now=1000,
            last_dispatched_at=None,
            cool_off_seconds=300,
            active_work=False,
            dry_run=False,
            dispatch_configured=False,
        )

        self.assertEqual(decision.name, "skipped")
        self.assertEqual(decision.reason, "dispatch-command-not-configured")


class InputValidationTests(unittest.TestCase):
    def test_non_path_configuration_is_rejected_as_value_error(self) -> None:
        with self.assertRaisesRegex(ValueError, "state_file"):
            configured_path([], "state_file")
        with self.assertRaisesRegex(ValueError, "log_file"):
            configured_path({}, "log_file")

    def test_non_string_repository_is_rejected_as_malformed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            path.write_text(
                '{"version":1,"repository":7,"pull_requests":{}}\n',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ValueError, "malformed state file"):
                load_state(path, "OWNER/REPOSITORY")

    def test_non_object_pull_request_record_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            path.write_text(
                '{"version":1,"repository":"OWNER/REPOSITORY",'
                '"pull_requests":{"17":[]}}\n',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ValueError, "malformed state file"):
                load_state(path, "OWNER/REPOSITORY")

    def test_non_finite_positive_number_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            positive_number(math.inf, "interval_seconds")

    def test_non_finite_nonnegative_number_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            nonnegative_number(math.nan, "cool_off_seconds")

    def test_non_numeric_configuration_is_rejected_as_value_error(self) -> None:
        with self.assertRaisesRegex(ValueError, "interval_seconds"):
            positive_number(None, "interval_seconds")
        with self.assertRaisesRegex(ValueError, "cool_off_seconds"):
            nonnegative_number([], "cool_off_seconds")

    def test_non_object_state_is_rejected_as_malformed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            path.write_text("[]\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "malformed state file"):
                load_state(path, "OWNER/REPOSITORY")


class GitHubGraphQLTests(unittest.TestCase):
    def test_resolution_observation_must_predate_next_review_request(self) -> None:
        thread = {
            "isResolved": True,
            "isDispositioned": True,
            "latestReviewerAt": "2026-08-16T10:00:00Z",
            "dispositionAt": "2026-08-16T10:01:00Z",
            "resolutionObservedAt": "2026-08-16T10:03:00Z",
        }

        eligible = prior_threads_dispositioned_before(
            [thread], "2026-08-16T10:02:00Z"
        )

        self.assertFalse(eligible)

    def test_persisted_review_requires_the_same_review_id(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "_persisted_record": {
                "authenticated_review_head": "head",
                "authenticated_review_id": "review-a",
                "authenticated_review_body": "description",
                "authenticated_review_check_inventory": [
                    "CheckRun:required build"
                ],
                "check_inventory": ["CheckRun:required build"],
            },
            "head_oid": "head",
            "body": "description",
            "authenticated_quiet_review_oids": [],
            "authenticated_review_ids": {},
            "observed_codex_reviews": {"review-b": "head"},
            "quiet_review_head_oids": [],
        }

        client._restore_persisted_review_evidence([pull_request])

        self.assertEqual(pull_request["authenticated_quiet_review_oids"], [])
        self.assertEqual(pull_request["quiet_review_head_oids"], [])

    def test_persisted_review_is_invalidated_by_description_edit(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "_persisted_record": {
                "authenticated_review_head": "head",
                "authenticated_review_id": "review-a",
                "authenticated_review_body": "reviewed description",
            },
            "head_oid": "head",
            "body": "edited description",
            "authenticated_quiet_review_oids": [],
            "authenticated_review_ids": {},
            "observed_codex_reviews": {"review-a": "head"},
            "quiet_review_head_oids": [],
        }

        client._restore_persisted_review_evidence([pull_request])

        self.assertEqual(pull_request["authenticated_quiet_review_oids"], [])
        self.assertEqual(pull_request["quiet_review_head_oids"], [])

    def test_persisted_review_survives_same_head_check_rerun(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "_persisted_record": {
                "authenticated_review_head": "head",
                "authenticated_review_id": "review-a",
                "authenticated_review_body": "description",
                "authenticated_review_check_inventory": [
                    "CheckRun:required build"
                ],
                "check_inventory": ["CheckRun:required build"],
            },
            "head_oid": "head",
            "body": "description",
            "authenticated_quiet_review_oids": [],
            "authenticated_review_ids": {},
            # A rerun of gating checks advances their completedAt past the
            # original request, so the fresh recomputation this tick no
            # longer finds a qualifying request for review-a.
            "observed_codex_reviews": {},
            "live_codex_review_oids": {"review-a": "head"},
            "checks": [
                {
                    "__typename": "CheckRun",
                    "name": "required build",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                }
            ],
            "check_rollup_state": "SUCCESS",
            "quiet_review_head_oids": [],
        }

        client._restore_persisted_review_evidence([pull_request])

        self.assertEqual(
            pull_request["authenticated_quiet_review_oids"], ["head"]
        )
        self.assertEqual(pull_request["quiet_review_head_oids"], ["head"])
        self.assertEqual(
            pull_request["authenticated_review_ids"]["head"], "review-a"
        )

    def test_persisted_review_is_invalidated_by_new_check_identity(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "_persisted_record": {
                "authenticated_review_head": "head",
                "authenticated_review_id": "review-a",
                "authenticated_review_body": "description",
                "authenticated_review_check_inventory": ["CheckRun:fast"],
                "check_inventory": ["CheckRun:fast", "CheckRun:late"],
            },
            "head_oid": "head",
            "body": "description",
            "authenticated_quiet_review_oids": [],
            "authenticated_review_ids": {},
            "observed_codex_reviews": {},
            "live_codex_review_oids": {"review-a": "head"},
            "checks": [
                {
                    "__typename": "CheckRun",
                    "name": name,
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                }
                for name in ("fast", "late")
            ],
            "check_rollup_state": "SUCCESS",
            "quiet_review_head_oids": [],
        }

        client._restore_persisted_review_evidence([pull_request])

        self.assertEqual(pull_request["authenticated_quiet_review_oids"], [])
        self.assertEqual(pull_request["quiet_review_head_oids"], [])

    def test_persisted_review_is_invalidated_by_a_body_only_finding(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        inventory = ["CheckRun:required build"]
        pull_request = {
            "_persisted_record": {
                "authenticated_review_head": "head",
                "authenticated_review_id": "review-a",
                "authenticated_review_body": "description",
                "authenticated_review_check_inventory": inventory,
                "check_inventory": inventory,
            },
            "head_oid": "head",
            "body": "description",
            "authenticated_quiet_review_oids": [],
            "authenticated_review_ids": {},
            "observed_codex_reviews": {"review-a": "head"},
            "live_codex_review_oids": {},
            "checks": [
                {
                    "__typename": "CheckRun",
                    "name": "required build",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                }
            ],
            "check_rollup_state": "SUCCESS",
            "quiet_review_head_oids": [],
        }

        client._restore_persisted_review_evidence([pull_request])

        self.assertEqual(pull_request["authenticated_quiet_review_oids"], [])
        self.assertEqual(pull_request["quiet_review_head_oids"], [])

    def test_persisted_review_can_authenticate_an_exempt_head_delta(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        inventory = ["CheckRun:required build"]
        pull_request = {
            "_persisted_record": {
                "authenticated_review_head": "reviewed-head",
                "authenticated_review_id": "review-a",
                "authenticated_review_body": "description",
                "authenticated_review_check_inventory": inventory,
                "check_inventory": inventory,
            },
            "head_oid": "advanced-head",
            "body": "description",
            "authenticated_quiet_review_oids": [],
            "authenticated_review_ids": {},
            "observed_codex_reviews": {},
            "live_codex_review_oids": {"review-a": "reviewed-head"},
            "checks": [
                {
                    "__typename": "CheckRun",
                    "name": "required build",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                }
            ],
            "check_rollup_state": "SUCCESS",
            "quiet_review_head_oids": [],
        }

        client._restore_persisted_review_evidence([pull_request])

        self.assertEqual(
            pull_request["authenticated_quiet_review_oids"], ["reviewed-head"]
        )
        self.assertEqual(pull_request["quiet_review_head_oids"], [])

    def test_check_rerun_restores_authenticated_review_to_wave_census(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        head = "a" * 40
        inventory = ["CheckRun:required build"]
        pull_request = {
            "_persisted_record": {
                "authenticated_review_head": head,
                "authenticated_review_id": "review-a",
                "authenticated_review_body": "description",
                "authenticated_review_check_inventory": inventory,
                "check_inventory": inventory,
            },
            "head_oid": head,
            "body": "description",
            "body_last_edited_at": None,
            "check_rollup_state": "SUCCESS",
            "checks": [
                {
                    "__typename": "CheckRun",
                    "name": "required build",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                    "completedAt": "2026-08-16T10:03:00Z",
                }
            ],
            "review_threads": [],
            "_review_comments": [
                {
                    "authorAssociation": "OWNER",
                    "body": f"@codex review\nExact head {head}",
                    "createdAt": "2026-08-16T10:00:00Z",
                }
            ],
            "_reviews": [
                {
                    "id": "review-a",
                    "author": {"login": "chatgpt-codex-connector"},
                    "state": "COMMENTED",
                    "body": "",
                    "submittedAt": "2026-08-16T10:01:00Z",
                    "commit": {"oid": head},
                    "comments": {"totalCount": 0},
                }
            ],
            "_thumbs_up_reactions": [],
        }

        client._finalize_review_evidence([pull_request])

        self.assertEqual(pull_request["_codex_reviews"], [])
        self.assertEqual(pull_request["observed_codex_reviews"], {})

        client._restore_persisted_review_evidence([pull_request])

        self.assertEqual(
            [review["id"] for review in pull_request["_codex_reviews"]],
            ["review-a"],
        )

    def test_persisted_review_not_restored_when_no_longer_live(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "_persisted_record": {
                "authenticated_review_head": "head",
                "authenticated_review_id": "review-a",
                "authenticated_review_body": "description",
            },
            "head_oid": "head",
            "body": "description",
            "authenticated_quiet_review_oids": [],
            "authenticated_review_ids": {},
            "observed_codex_reviews": {},
            # review-a is no longer a live Codex review (e.g. dismissed),
            # so a check rerun on the same head must not resurrect it.
            "live_codex_review_oids": {},
            "checks": [
                {
                    "__typename": "CheckRun",
                    "name": "required build",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                }
            ],
            "check_rollup_state": "SUCCESS",
            "quiet_review_head_oids": [],
        }

        client._restore_persisted_review_evidence([pull_request])

        self.assertEqual(pull_request["authenticated_quiet_review_oids"], [])
        self.assertEqual(pull_request["quiet_review_head_oids"], [])

    def test_persisted_review_not_restored_when_rerun_checks_still_failing(
        self,
    ) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "_persisted_record": {
                "authenticated_review_head": "head",
                "authenticated_review_id": "review-a",
                "authenticated_review_body": "description",
            },
            "head_oid": "head",
            "body": "description",
            "authenticated_quiet_review_oids": [],
            "authenticated_review_ids": {},
            "observed_codex_reviews": {},
            "live_codex_review_oids": {"review-a": "head"},
            "checks": [
                {
                    "__typename": "CheckRun",
                    "name": "required build",
                    "status": "COMPLETED",
                    "conclusion": "FAILURE",
                }
            ],
            "check_rollup_state": "FAILURE",
            "quiet_review_head_oids": [],
        }

        client._restore_persisted_review_evidence([pull_request])

        self.assertEqual(pull_request["authenticated_quiet_review_oids"], [])
        self.assertEqual(pull_request["quiet_review_head_oids"], [])

    def test_material_base_forward_resets_escalation_wave_count(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        reviews = [
            {
                "id": "review-1",
                "author": {"login": "chatgpt-codex-connector"},
                "submittedAt": "2026-08-16T10:01:00Z",
            },
            {
                "id": "review-2",
                "author": {"login": "chatgpt-codex-connector"},
                "submittedAt": "2026-08-16T10:02:00Z",
            },
            {
                "id": "review-3",
                "author": {"login": "chatgpt-codex-connector"},
                "submittedAt": "2026-08-16T10:03:00Z",
            },
            {
                "id": "review-4",
                "author": {"login": "chatgpt-codex-connector"},
                "submittedAt": "2026-08-16T10:04:00Z",
            },
            {
                "id": "review-5",
                "author": {"login": "chatgpt-codex-connector"},
                "submittedAt": "2026-08-16T10:05:00Z",
            },
        ]
        pull_request = {
            "_persisted_record": {
                "head_oid": "reviewed-head",
                "known_codex_review_ids": [
                    "review-1", "review-2", "review-3", "review-4"
                ],
                "review_wave_ids": [
                    "review-1", "review-2", "review-3", "review-4"
                ],
                "review_wave_base_oid": "base-old",
            },
            "_codex_reviews": reviews,
            "base_oid": "base-new",
            "head_oid": "head-new",
            "review_threads": [
                {
                    "dispositionKind": "escalated",
                    "isDispositioned": True,
                    "isEscalated": True,
                    "reviewIds": ["review-5"],
                }
            ],
        }
        with mock.patch.object(
            client, "_review_exempt_change", return_value=False
        ):
            client._validate_review_waves([pull_request])

        thread = pull_request["review_threads"][0]
        self.assertEqual(pull_request["review_wave_ids"], ["review-5"])
        self.assertFalse(thread["isDispositioned"])
        self.assertFalse(thread["isEscalated"])

    def test_base_only_advance_preserves_review_waves(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "_persisted_record": {
                "head_oid": "unchanged-head",
                "known_codex_review_ids": ["review-1"],
                "review_wave_ids": ["review-1"],
                "review_wave_base_oid": "base-old",
            },
            "_codex_reviews": [
                {
                    "id": "review-1",
                    "author": {"login": "chatgpt-codex-connector"},
                    "submittedAt": "2026-08-16T10:01:00Z",
                }
            ],
            "base_oid": "base-new",
            "head_oid": "unchanged-head",
            "review_threads": [],
        }
        with mock.patch.object(client, "_review_exempt_change") as exempt:
            client._validate_review_waves([pull_request])

        exempt.assert_not_called()
        self.assertEqual(pull_request["review_wave_ids"], ["review-1"])
        self.assertEqual(pull_request["review_wave_base_oid"], "base-old")

    def test_body_only_codex_finding_is_not_quiet_review_evidence(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        head = "a" * 40
        pull_request = {
            "head_oid": head,
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "review_threads": [],
            "_review_comments": [
                {
                    "authorAssociation": "OWNER",
                    "body": f"@codex review\nExact head {head}",
                    "createdAt": "2026-08-16T10:00:00Z",
                }
            ],
            "_reviews": [
                {
                    "id": "review-node",
                    "author": {"login": "chatgpt-codex-connector"},
                    "state": "COMMENTED",
                    "body": "P1: this review contains a body-only finding",
                    "submittedAt": "2026-08-16T10:01:00Z",
                    "commit": {"oid": head},
                    "comments": {"totalCount": 0},
                }
            ],
        }

        client._finalize_review_evidence([pull_request])

        self.assertEqual(pull_request["quiet_review_head_oids"], [])
        self.assertEqual(pull_request["live_codex_review_oids"], {})

    def test_review_body_finding_is_not_hidden_by_declined_threads(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        head = "a" * 40
        pull_request = {
            "head_oid": head,
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "review_threads": [
                {
                    "isResolved": True,
                    "isDispositioned": True,
                    "isInformational": False,
                    "dispositionKind": "declined",
                    "reviewIds": ["review-node"],
                }
            ],
            "_review_comments": [
                {
                    "authorAssociation": "OWNER",
                    "body": f"@codex review\nExact head {head}",
                    "createdAt": "2026-08-16T10:00:00Z",
                }
            ],
            "_reviews": [
                {
                    "id": "review-node",
                    "author": {"login": "chatgpt-codex-connector"},
                    "state": "COMMENTED",
                    "body": "P1: an additional body finding",
                    "submittedAt": "2026-08-16T10:01:00Z",
                    "commit": {"oid": head},
                    "comments": {"totalCount": 1},
                }
            ],
        }

        client._finalize_review_evidence([pull_request])

        self.assertEqual(pull_request["quiet_review_head_oids"], [])
        self.assertEqual(pull_request["live_codex_review_oids"], {})

    def test_completed_summary_and_reaction_authenticate_quiet_review(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        head = "a" * 40
        pull_request = {
            "head_oid": head,
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "review_threads": [],
            "_review_comments": [
                {
                    "id": "request",
                    "authorAssociation": "OWNER",
                    "body": f"@codex review\nExact head {head}",
                    "createdAt": "2026-09-05T18:30:00Z",
                },
                {
                    "id": "summary",
                    "author": {"login": "chatgpt-codex-connector"},
                    "body": (
                        "<!-- codex-pull-request-review-summary -->\n"
                        "| 📝 **Code Review** | ✅ **Completed** "
                        '<relative-time datetime="2026-09-05T18:35:26.344586Z">'
                        "now</relative-time> | `aaaaaaaaaa` | User request |"
                    ),
                    "createdAt": "2026-09-05T17:02:17Z",
                    "lastEditedAt": "2026-09-05T18:35:26Z",
                },
            ],
            "_thumbs_up_reactions": [
                {
                    "user": {"login": "chatgpt-codex-connector[bot]"},
                    "createdAt": "2026-09-05T18:35:29Z",
                }
            ],
            "_reviews": [],
        }

        client._finalize_review_evidence([pull_request])

        self.assertEqual(pull_request["quiet_review_head_oids"], [head])
        self.assertEqual(
            pull_request["authenticated_review_ids"], {head: "summary"}
        )

    def test_description_edit_after_review_invalidates_fresh_evidence(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        head = "a" * 40
        pull_request = {
            "head_oid": head,
            "body_last_edited_at": "2026-08-16T10:02:00Z",
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "review_threads": [],
            "_review_comments": [
                {
                    "authorAssociation": "OWNER",
                    "body": f"@codex review\nExact head {head}",
                    "createdAt": "2026-08-16T10:00:00Z",
                }
            ],
            "_reviews": [
                {
                    "id": "review-node",
                    "author": {"login": "chatgpt-codex-connector"},
                    "submittedAt": "2026-08-16T10:01:00Z",
                    "commit": {"oid": head},
                    "comments": {"totalCount": 0},
                }
            ],
        }

        client._finalize_review_evidence([pull_request])

        self.assertEqual(pull_request["quiet_review_head_oids"], [])

    def test_fixing_commit_verification_failure_aborts_snapshot(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "head_oid": "head",
            "review_threads": [
                {
                    "dispositionKind": "fixed",
                    "fixingCommit": "abcdef123",
                    "isDispositioned": True,
                }
            ],
        }
        with mock.patch.object(
            client, "execute_rest", side_effect=RuntimeError("unavailable")
        ):
            with self.assertRaisesRegex(RuntimeError, "unavailable"):
                client._validate_fixing_commits([pull_request])

    def test_missing_fixing_commit_is_invalid_disposition_not_snapshot_abort(
        self,
    ) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "head_oid": "head",
            "review_threads": [
                {
                    "dispositionKind": "fixed",
                    "fixingCommit": "nonexistentcommit",
                    "isDispositioned": True,
                }
            ],
        }
        with mock.patch.object(
            client,
            "execute_rest",
            side_effect=GitHubNotFoundError("gh REST request failed: not found"),
        ):
            client._validate_fixing_commits([pull_request])

        thread = pull_request["review_threads"][0]
        self.assertFalse(thread["isDispositioned"])
        self.assertIsNone(thread["dispositionKind"])
        self.assertIsNone(thread["fixingCommit"])

    def test_one_missing_fixing_commit_does_not_abort_other_threads(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "head_oid": "head",
            "review_threads": [
                {
                    "dispositionKind": "fixed",
                    "fixingCommit": "nonexistentcommit",
                    "isDispositioned": True,
                },
                {
                    "dispositionKind": "fixed",
                    "fixingCommit": "realcommit",
                    "isDispositioned": True,
                },
            ],
        }

        def fake_execute_rest(path: str):
            if "nonexistentcommit" in path:
                raise GitHubNotFoundError("gh REST request failed: not found")
            return {
                "status": "ahead",
                "base_commit": {"sha": "merge-base"},
                "head_commit": {"sha": "head"},
                "merge_base_commit": {"sha": "merge-base"},
            }

        with mock.patch.object(
            client, "execute_rest", side_effect=fake_execute_rest
        ):
            client._validate_fixing_commits([pull_request])

        invalid_thread, valid_thread = pull_request["review_threads"]
        self.assertFalse(invalid_thread["isDispositioned"])
        self.assertTrue(valid_thread["isDispositioned"])
        self.assertEqual(valid_thread["dispositionKind"], "fixed")

    def test_non_python_c_header_is_not_comment_only(self) -> None:
        changed_file = {
            "filename": "include/config.h",
            "patch": "@@ -1 +1 @@\n-#define LIMIT 4\n+#define LIMIT 8",
        }

        self.assertFalse(comment_only_patch(changed_file))

    def test_non_python_rust_file_is_not_comment_only(self) -> None:
        changed_file = {
            "filename": "src/value.rs",
            "patch": "@@ -1 +1 @@\n-*value = 4;\n+*value = 8;",
        }

        self.assertFalse(comment_only_patch(changed_file))

    def test_python_hash_comments_are_comment_only(self) -> None:
        changed_file = {
            "filename": "tool.py",
            "patch": "@@ -1 +1 @@\n-# old explanation\n+# new explanation",
        }

        self.assertTrue(comment_only_patch(changed_file))

    def test_python_string_contents_are_not_comment_only(self) -> None:
        changed_file = {
            "filename": "tool.py",
            "patch": (
                "@@ -1,3 +1,3 @@\n"
                " text = \"\"\"\n"
                "-# old string content\n"
                "+# new string content\n"
                " \"\"\""
            ),
        }

        self.assertFalse(comment_only_patch(changed_file))

    def test_python_executable_change_is_not_comment_only(self) -> None:
        changed_file = {
            "filename": "tool.py",
            "patch": "@@ -1,2 +1,2 @@\n # explanation\n-old = 1\n+new = 2",
        }

        self.assertFalse(comment_only_patch(changed_file))

    def test_python_encoding_cookie_change_is_not_comment_only(self) -> None:
        changed_file = {
            "filename": "tool.py",
            "patch": (
                "@@ -1 +1 @@\n"
                "-# -*- coding: utf-8 -*-\n"
                "+# -*- coding: latin-1 -*-"
            ),
        }

        self.assertFalse(comment_only_patch(changed_file))

    def test_python_shebang_change_is_not_comment_only(self) -> None:
        changed_file = {
            "filename": "tool.py",
            "patch": (
                "@@ -1 +1 @@\n"
                "-#!/usr/bin/env python3\n"
                "+#!/usr/bin/env python2"
            ),
        }

        self.assertFalse(comment_only_patch(changed_file))

    def test_python_hunk_content_that_looks_like_a_header_is_not_comment_only(
        self,
    ) -> None:
        changed_file = {
            "filename": "tool.py",
            "patch": "@@ -1 +1 @@\n-# explanation\n+++danger()",
        }

        self.assertFalse(comment_only_patch(changed_file))

    def test_edited_rename_is_not_review_exempt(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        comparison = {
            "total_commits": 1,
            "commits": [{"sha": "head"}],
            "files": [
                {
                    "status": "renamed",
                    "changes": 2,
                    "additions": 1,
                    "deletions": 1,
                    "filename": "src/new.rs",
                    "patch": "@@ -1 +1 @@\n-let old = 1;\n+let new = 2;",
                }
            ],
        }
        with mock.patch.object(client, "execute_rest", return_value=comparison):
            exempt = client._review_exempt_change("reviewed", "head", "base")

        self.assertFalse(exempt)

    def test_unverified_merge_commit_is_not_review_exempt(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        comparison = {
            "total_commits": 1,
            "commits": [
                {
                    "sha": "head",
                    "parents": [{"sha": "reviewed"}, {"sha": "other"}],
                }
            ],
            "files": [
                {
                    "status": "modified",
                    "filename": "src/value.rs",
                    "patch": "@@ -1 +1 @@\n-let old = 1;\n+let new = 2;",
                }
            ],
        }
        with mock.patch.object(client, "execute_rest", return_value=comparison):
            exempt = client._review_exempt_change("reviewed", "head", "base")

        self.assertFalse(exempt)

    def test_exact_clean_base_forward_is_review_exempt(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        base_delta = {
            "filename": "workflow.yml",
            "status": "modified",
            "additions": 1,
            "deletions": 1,
            "changes": 2,
            "sha": "base-blob",
            "patch": "@@ -1 +1 @@\n-old\n+new",
        }
        comparison = {
            "total_commits": 1,
            "commits": [
                {
                    "sha": "head",
                    "parents": [{"sha": "reviewed"}, {"sha": "base"}],
                }
            ],
            "files": [base_delta],
        }
        reviewed_to_base = {"merge_base_commit": {"sha": "common-base"}}
        base_comparison = {
            "total_commits": 1,
            "commits": [{"sha": "base"}],
            "files": [base_delta],
        }
        with mock.patch.object(
            client,
            "execute_rest",
            side_effect=[comparison, reviewed_to_base, base_comparison],
        ):
            exempt = client._review_exempt_change("reviewed", "head", "base")

        self.assertTrue(exempt)

    def test_merge_forward_with_conflict_edit_is_not_review_exempt(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        comparison = {
            "total_commits": 1,
            "commits": [
                {
                    "sha": "head",
                    "parents": [{"sha": "reviewed"}, {"sha": "base"}],
                }
            ],
            "files": [
                {
                    "filename": "workflow.yml",
                    "status": "modified",
                    "additions": 1,
                    "deletions": 1,
                    "changes": 2,
                    "sha": "conflict-edit",
                    "patch": "@@ -1 +1 @@\n-old\n+unexpected",
                }
            ],
        }
        reviewed_to_base = {"merge_base_commit": {"sha": "common-base"}}
        base_comparison = {
            "total_commits": 1,
            "commits": [{"sha": "base"}],
            "files": [
                {
                    "filename": "workflow.yml",
                    "status": "modified",
                    "additions": 1,
                    "deletions": 1,
                    "changes": 2,
                    "sha": "base-blob",
                    "patch": "@@ -1 +1 @@\n-old\n+new",
                }
            ]
        }
        with mock.patch.object(
            client,
            "execute_rest",
            side_effect=[comparison, reviewed_to_base, base_comparison],
        ):
            exempt = client._review_exempt_change("reviewed", "head", "base")

        self.assertFalse(exempt)

    def test_unreachable_review_commit_does_not_abort_exemption_loading(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "authenticated_quiet_review_oids": ["reviewed", "unreachable"],
            "base_oid": "base",
            "head_oid": "head",
            "quiet_review_head_oids": [],
        }
        with mock.patch.object(
            client,
            "_review_exempt_change",
            side_effect=[GitHubNotFoundError("not found"), True],
        ) as review_exempt_change:
            client._load_review_exempt_status([pull_request])

        self.assertTrue(pull_request["review_exempt_since_quiet_review"])
        self.assertEqual(review_exempt_change.call_count, 2)

    def test_transient_review_compare_failure_aborts_exemption_loading(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "authenticated_quiet_review_oids": ["reviewed"],
            "base_oid": "base",
            "head_oid": "head",
            "quiet_review_head_oids": [],
        }
        with mock.patch.object(
            client,
            "_review_exempt_change",
            side_effect=RuntimeError("compare timed out"),
        ):
            with self.assertRaisesRegex(RuntimeError, "timed out"):
                client._load_review_exempt_status([pull_request])

    def test_change_request_review_is_only_wave_evidence(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        head = "a" * 40
        pull_request = {
            "head_oid": head,
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "review_threads": [],
            "_review_comments": [
                {
                    "authorAssociation": "OWNER",
                    "body": f"@codex review\nExact head {head}",
                    "createdAt": "2026-08-16T10:00:00Z",
                }
            ],
            "_reviews": [
                {
                    "id": "review-node",
                    "author": {"login": "chatgpt-codex-connector"},
                    "state": "CHANGES_REQUESTED",
                    "submittedAt": "2026-08-16T10:01:00Z",
                    "commit": {"oid": head},
                    "comments": {"totalCount": 0},
                }
            ],
        }

        client._finalize_review_evidence([pull_request])

        self.assertEqual(pull_request["quiet_review_head_oids"], [])
        self.assertEqual(
            pull_request["observed_codex_reviews"], {"review-node": head}
        )

    def test_codex_review_without_authenticated_request_is_not_wave_evidence(
        self,
    ) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        head = "a" * 40
        pull_request = {
            "head_oid": head,
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "review_threads": [],
            "_review_comments": [],
            "_reviews": [
                {
                    "id": "review-node",
                    "author": {"login": "chatgpt-codex-connector"},
                    "state": "COMMENTED",
                    "submittedAt": "2026-08-16T10:01:00Z",
                    "commit": {"oid": head},
                    "comments": {"totalCount": 0},
                }
            ],
        }

        client._finalize_review_evidence([pull_request])

        self.assertEqual(pull_request["observed_codex_reviews"], {})

    def test_oid_revalidation_can_abort_on_change(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "node_id": "node",
            "base_ref": "main",
            "base_oid": "base",
            "head_ref": "agent/work",
            "head_oid": "old-head",
            "is_draft": False,
            "body": "body",
            "body_last_edited_at": None,
            "mergeable": "MERGEABLE",
            "review_decision": None,
        }
        response = {
            "item0": {
                "state": "OPEN",
                "baseRefName": "main",
                "baseRefOid": "base",
                "headRefName": "agent/work",
                "headRefOid": "new-head",
                "isDraft": False,
                "body": "body",
                "lastEditedAt": None,
                "mergeable": "MERGEABLE",
                "reviewDecision": None,
            }
        }
        with mock.patch.object(client, "execute", return_value=response):
            with self.assertRaisesRegex(RuntimeError, "changed after"):
                client._verify_snapshot_oids([pull_request], raise_on_change=True)

    def test_final_revalidation_detects_description_change(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "node_id": "node",
            "base_ref": "main",
            "base_oid": "base",
            "head_ref": "agent/work",
            "head_oid": "head",
            "is_draft": False,
            "body": "old body",
            "body_last_edited_at": "2026-09-05T00:00:00Z",
            "mergeable": "MERGEABLE",
            "review_decision": None,
        }
        response = {
            "item0": {
                "state": "OPEN",
                "baseRefName": "main",
                "baseRefOid": "base",
                "headRefName": "agent/work",
                "headRefOid": "head",
                "isDraft": False,
                "body": "new body",
                "lastEditedAt": "2026-09-05T00:01:00Z",
                "mergeable": "MERGEABLE",
                "reviewDecision": None,
            }
        }

        with mock.patch.object(client, "execute", return_value=response):
            with self.assertRaisesRegex(RuntimeError, "changed after"):
                client._verify_snapshot_oids(
                    [pull_request], raise_on_change=True
                )

    def test_check_inventory_requires_same_nonempty_consecutive_set(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "_prior_check_inventory": ["CheckRun:required"],
            "checks": [
                {
                    "__typename": "CheckRun",
                    "name": "required",
                }
            ],
        }

        client._finalize_check_inventory([pull_request])

        self.assertEqual(
            pull_request["check_inventory"], ["CheckRun:required"]
        )
        self.assertTrue(pull_request["check_inventory_stable"])

    def test_all_declined_review_completes_terminal_wave(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        head = "a" * 40
        pull_request = {
            "head_oid": head,
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "review_threads": [
                {
                    "isResolved": True,
                    "isDispositioned": True,
                    "isEscalated": False,
                    "dispositionKind": "declined",
                    "reviewIds": ["review-node"],
                }
            ],
            "_review_comments": [
                {
                    "authorAssociation": "OWNER",
                    "body": f"@codex review\nExact head {head}",
                    "createdAt": "2026-08-16T10:00:00Z",
                }
            ],
            "_reviews": [
                {
                    "id": "review-node",
                    "author": {"login": "chatgpt-codex-connector"},
                    "submittedAt": "2026-08-16T10:01:00Z",
                    "commit": {"oid": head},
                    "comments": {"totalCount": 1},
                }
            ],
        }

        client._finalize_review_evidence([pull_request])

        self.assertEqual(pull_request["quiet_review_head_oids"], [head])

    def test_dismissed_review_is_not_quiet_review_evidence(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        head = "a" * 40
        pull_request = {
            "head_oid": head,
            "check_rollup_state": "SUCCESS",
            "checks": [],
            "review_threads": [],
            "_review_comments": [
                {
                    "authorAssociation": "OWNER",
                    "body": f"@codex review\nExact head {head}",
                    "createdAt": "2026-08-16T10:00:00Z",
                }
            ],
            "_reviews": [
                {
                    "id": "dismissed-review",
                    "author": {"login": "chatgpt-codex-connector"},
                    "state": "DISMISSED",
                    "submittedAt": "2026-08-16T10:01:00Z",
                    "commit": {"oid": head},
                    "comments": {"totalCount": 0},
                }
            ],
        }

        client._finalize_review_evidence([pull_request])

        self.assertEqual(pull_request["quiet_review_head_oids"], [])
        self.assertEqual(pull_request["observed_codex_reviews"], {})

    def test_review_requests_and_reviews_are_paginated(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "node_id": "pull-request-node",
            "_review_comments": [{"body": "first comment"}],
            "_reviews": [{"id": "first-review"}],
            "_review_comment_page": {
                "hasNextPage": True,
                "endCursor": "comment-cursor",
            },
            "_review_page": {
                "hasNextPage": True,
                "endCursor": "review-cursor",
            },
        }
        response = {
            "node": {
                "comments": {
                    "nodes": [{"body": "later request"}],
                    "pageInfo": {"hasNextPage": False, "endCursor": None},
                },
                "reviews": {
                    "nodes": [{"id": "later-review"}],
                    "pageInfo": {"hasNextPage": False, "endCursor": None},
                },
            }
        }
        with mock.patch.object(client, "execute", return_value=response) as execute:
            client._finish_review_evidence_connections([pull_request])

        self.assertEqual(len(pull_request["_review_comments"]), 2)
        self.assertEqual(len(pull_request["_reviews"]), 2)
        self.assertEqual(execute.call_args.args[1]["commentsAfter"], "comment-cursor")
        self.assertEqual(execute.call_args.args[1]["reviewsAfter"], "review-cursor")

    def test_answered_informational_thread_is_dispositioned(self) -> None:
        threads = [
            {
                "isResolved": True,
                "comments": {
                    "nodes": [
                        {
                            "author": {"login": "reviewer"},
                            "body": "Question: why is this interval configurable?",
                        },
                        {
                            "author": {"login": "owner"},
                            "authorAssociation": "OWNER",
                            "body": "Operators need different polling budgets.",
                        },
                    ]
                },
            }
        ]

        normalized = normalize_review_threads(threads, "owner")

        self.assertTrue(normalized[0]["isDispositioned"])

    def test_question_phrased_finding_is_not_informational(self) -> None:
        threads = [
            {
                "isResolved": True,
                "comments": {
                    "nodes": [
                        {
                            "author": {"login": "reviewer"},
                            "body": "This double-dispatches drafts forever, can you confirm?",
                        },
                        {
                            "author": {"login": "owner"},
                            "authorAssociation": "OWNER",
                            "body": "ack",
                        },
                    ]
                },
            }
        ]

        normalized = normalize_review_threads(threads, "owner")

        self.assertFalse(normalized[0]["isInformational"])
        self.assertFalse(normalized[0]["isDispositioned"])

    def test_trivial_informational_reply_is_not_an_answer(self) -> None:
        threads = [
            {
                "isResolved": True,
                "comments": {
                    "nodes": [
                        {
                            "author": {"login": "reviewer"},
                            "body": "Question: why is this interval configurable?",
                        },
                        {
                            "author": {"login": "owner"},
                            "authorAssociation": "OWNER",
                            "body": "ack",
                        },
                    ]
                },
            }
        ]

        normalized = normalize_review_threads(threads, "owner")

        self.assertTrue(normalized[0]["isInformational"])
        self.assertFalse(normalized[0]["isDispositioned"])

    def test_reply_must_follow_latest_reviewer_comment(self) -> None:
        threads = [
            {
                "isResolved": True,
                "comments": {
                    "nodes": [
                        {"author": {"login": "reviewer"}, "body": "Finding"},
                        {
                            "author": {"login": "owner"},
                            "authorAssociation": "OWNER",
                            "body": "Fixed in commit `abcdef123`.",
                        },
                        {
                            "author": {"login": "reviewer"},
                            "body": "The edge case still reproduces.",
                        },
                    ]
                },
            }
        ]

        normalized = normalize_review_threads(threads, "owner")

        self.assertFalse(normalized[0]["isDispositioned"])

    def test_thread_comment_pagination_finds_late_disposition(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "author_login": "owner",
            "review_threads": [],
            "_review_thread_nodes": [
                {
                    "id": "thread-node",
                    "isResolved": True,
                    "comments": {
                        "nodes": [
                            {
                                "author": {"login": "reviewer"},
                                "body": "Finding",
                            }
                        ],
                        "pageInfo": {
                            "hasNextPage": True,
                            "endCursor": "cursor-100",
                        },
                    },
                }
            ],
        }
        response = {
            "node": {
                "comments": {
                    "nodes": [
                        {
                            "author": {"login": "owner"},
                            "authorAssociation": "OWNER",
                            "body": "Fixed in commit `abcdef123`.",
                        }
                    ],
                    "pageInfo": {
                        "hasNextPage": False,
                        "endCursor": None,
                    },
                }
            }
        }
        with mock.patch.object(client, "execute", return_value=response) as execute:
            client._finish_thread_comments([pull_request])

        self.assertTrue(pull_request["review_threads"][0]["isDispositioned"])
        self.assertEqual(execute.call_count, 1)

    def test_renamed_planning_file_uses_previous_base_path(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "base_oid": "base",
            "head_oid": "head",
            "changed_files": [
                {
                    "path": "docs/agents/new-name.md",
                    "previous_path": "docs/agents/old-name.md",
                    "changeType": "RENAMED",
                }
            ],
        }
        banner = (
            "# Work backlog\n\n"
            "> **Non-authoritative planning scratchpad — do not review for consistency.**\n"
        )
        response = {
            "repository": {
                "head": {"text": banner},
                "base": {"text": banner},
            }
        }
        with mock.patch.object(client, "execute", return_value=response) as execute:
            client._load_planning_only_status([pull_request])

        variables = execute.call_args.args[1]
        self.assertEqual(variables["base"], "base:docs/agents/old-name.md")
        self.assertTrue(pull_request["planning_only"])

    def test_review_request_before_green_ci_is_not_accepted(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "head_oid": "a" * 40,
            "check_rollup_state": "SUCCESS",
            "checks": [
                {
                    "__typename": "CheckRun",
                    "name": "required test",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                    "completedAt": "2026-08-16T10:05:00Z",
                }
            ],
            "_review_comments": [
                {
                    "authorAssociation": "OWNER",
                    "body": "@codex review\nExact head " + "a" * 40,
                    "createdAt": "2026-08-16T10:00:00Z",
                }
            ],
            "_reviews": [
                {
                    "author": {"login": "chatgpt-codex-connector"},
                    "submittedAt": "2026-08-16T10:06:00Z",
                    "commit": {"oid": "a" * 40},
                    "comments": {"totalCount": 0},
                }
            ],
        }

        client._finalize_review_evidence([pull_request])

        self.assertEqual(pull_request["quiet_review_head_oids"], [])

    def test_unavailable_repository_is_a_runtime_error(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        response = {"repository": None, "tracked": []}
        with mock.patch.object(client, "execute", return_value=response):
            with self.assertRaisesRegex(RuntimeError, "repository is unavailable"):
                client.snapshot([], "agent/*")

    def test_acknowledgement_is_not_a_finding_disposition(self) -> None:
        threads = [
            {
                "isResolved": True,
                "comments": {
                    "nodes": [
                        {"author": {"login": "reviewer"}, "body": "Finding"},
                        {
                            "author": {"login": "owner"},
                            "authorAssociation": "OWNER",
                            "body": "ack",
                        },
                    ]
                },
            }
        ]

        normalized = normalize_review_threads(threads, "owner")

        self.assertEqual(
            normalized,
            [
                {
                    "isResolved": True,
                    "isDispositioned": False,
                    "isEscalated": False,
                    "isInformational": False,
                    "latestReviewerAt": None,
                    "dispositionAt": None,
                    "dispositionKind": None,
                    "fixingCommit": None,
                    "reviewIds": [],
                }
            ],
        )

    def test_fix_reply_with_commit_is_a_finding_disposition(self) -> None:
        threads = [
            {
                "isResolved": True,
                "comments": {
                    "nodes": [
                        {"author": {"login": "reviewer"}, "body": "Finding"},
                        {
                            "author": {"login": "owner"},
                            "authorAssociation": "OWNER",
                            "body": "Fixed in commit `abcdef123`.",
                        },
                    ]
                },
            }
        ]

        normalized = normalize_review_threads(threads, "owner")

        self.assertEqual(
            normalized,
            [
                {
                    "isResolved": True,
                    "isDispositioned": True,
                    "isEscalated": False,
                    "isInformational": False,
                    "latestReviewerAt": None,
                    "dispositionAt": None,
                    "dispositionKind": "fixed",
                    "fixingCommit": "abcdef123",
                    "reviewIds": [],
                }
            ],
        )

    def test_trusted_reply_review_record_remains_a_disposition(self) -> None:
        threads = [
            {
                "isResolved": True,
                "comments": {
                    "nodes": [
                        {
                            "author": {"login": "reviewer"},
                            "authorAssociation": "NONE",
                            "body": "Finding",
                            "createdAt": "2026-09-05T10:00:00Z",
                            "pullRequestReview": {"id": "review-finding"},
                        },
                        {
                            "author": {"login": "owner"},
                            "authorAssociation": "OWNER",
                            "body": "Fixed in commit abcdef123.",
                            "createdAt": "2026-09-05T10:05:00Z",
                            "pullRequestReview": {"id": "review-reply"},
                        },
                    ]
                },
            }
        ]

        normalized = normalize_review_threads(threads, "owner")[0]

        self.assertTrue(normalized["isDispositioned"])
        self.assertEqual(normalized["dispositionKind"], "fixed")
        self.assertEqual(
            normalized["latestReviewerAt"], "2026-09-05T10:00:00Z"
        )

    def test_trusted_reviewer_follow_up_invalidates_an_earlier_disposition(
        self,
    ) -> None:
        threads = [
            {
                "isResolved": True,
                "comments": {
                    "nodes": [
                        {
                            "author": {"login": "reviewer"},
                            "authorAssociation": "NONE",
                            "body": "Finding",
                            "createdAt": "2026-09-05T10:00:00Z",
                            "pullRequestReview": {"id": "review-finding"},
                        },
                        {
                            "author": {"login": "owner"},
                            "authorAssociation": "OWNER",
                            "body": "Fixed in commit abcdef123.",
                            "createdAt": "2026-09-05T10:05:00Z",
                            "pullRequestReview": {"id": "review-reply"},
                        },
                        {
                            "author": {"login": "member-reviewer"},
                            "authorAssociation": "MEMBER",
                            "body": "This remains incorrect.",
                            "createdAt": "2026-09-05T10:10:00Z",
                            "pullRequestReview": {"id": "review-follow-up"},
                        },
                    ]
                },
            }
        ]

        normalized = normalize_review_threads(threads, "owner")[0]

        self.assertFalse(normalized["isDispositioned"])
        self.assertIsNone(normalized["dispositionKind"])
        self.assertEqual(
            normalized["latestReviewerAt"], "2026-09-05T10:10:00Z"
        )

    def test_exact_codex_review_command_is_a_review_request(self) -> None:
        head = "a" * 40
        comment = {
            "authorAssociation": "OWNER",
            "body": f"@codex review the current head `{head}`",
        }

        self.assertTrue(is_codex_review_request(comment, head))

    def test_codex_review_command_typos_are_not_review_requests(self) -> None:
        head = "a" * 40
        for body in (
            f"@codex reviewer {head}",
            f"@codex review-later {head}",
            f"@codex reviews {head}",
        ):
            with self.subTest(body=body):
                comment = {"authorAssociation": "OWNER", "body": body}

                self.assertFalse(is_codex_review_request(comment, head))

    def test_edited_disposition_uses_the_effective_edit_time(self) -> None:
        threads = [
            {
                "isResolved": True,
                "comments": {
                    "nodes": [
                        {
                            "author": {"login": "reviewer"},
                            "body": "Finding",
                            "createdAt": "2026-01-01T00:00:00Z",
                        },
                        {
                            "author": {"login": "owner"},
                            "authorAssociation": "OWNER",
                            "body": "Declined: not applicable.",
                            "createdAt": "2026-01-02T00:00:00Z",
                            "lastEditedAt": "2026-01-09T00:00:00Z",
                        },
                    ]
                },
            }
        ]

        normalized = normalize_review_threads(threads, "owner")

        self.assertEqual(normalized[0]["dispositionKind"], "declined")
        self.assertEqual(
            normalized[0]["dispositionAt"], "2026-01-09T00:00:00Z"
        )

    def test_details_query_fetches_thread_comment_edit_time(self) -> None:
        self.assertIn(
            "authorAssociation body createdAt lastEditedAt pullRequestReview",
            PULL_REQUEST_DETAILS_QUERY,
        )

    def test_thread_list_pagination_fetches_comment_edit_time(self) -> None:
        query, _ = pagination_query(
            [PaginationTask("threads", {"node_id": "PR_thread_page"}, "cursor-100")]
        )

        self.assertIn(
            "authorAssociation body createdAt lastEditedAt pullRequestReview",
            query,
        )

    def test_thread_comment_pagination_fetches_comment_edit_time(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "author_login": "owner",
            "review_threads": [],
            "_review_thread_nodes": [
                {
                    "id": "thread-node",
                    "isResolved": True,
                    "comments": {
                        "nodes": [],
                        "pageInfo": {
                            "hasNextPage": True,
                            "endCursor": "cursor-100",
                        },
                    },
                }
            ],
        }
        response = {
            "node": {
                "comments": {
                    "nodes": [],
                    "pageInfo": {"hasNextPage": False, "endCursor": None},
                }
            }
        }
        with mock.patch.object(client, "execute", return_value=response) as execute:
            client._finish_thread_comments([pull_request])

        self.assertIn(
            "authorAssociation body createdAt lastEditedAt pullRequestReview",
            execute.call_args.args[0],
        )

    def test_escalation_marker_is_a_terminal_open_disposition(self) -> None:
        threads = [
            {
                "isResolved": False,
                "comments": {
                    "nodes": [
                        {"author": {"login": "reviewer"}, "body": "Finding"},
                        {
                            "author": {"login": "owner"},
                            "authorAssociation": "OWNER",
                            "body": "Escalated without disposition",
                        },
                    ]
                },
            }
        ]

        normalized = normalize_review_threads(threads, "owner")

        self.assertEqual(
            normalized,
            [
                {
                    "isResolved": False,
                    "isDispositioned": True,
                    "isEscalated": True,
                    "isInformational": False,
                    "latestReviewerAt": None,
                    "dispositionAt": None,
                    "dispositionKind": "escalated",
                    "fixingCommit": None,
                    "reviewIds": [],
                }
            ],
        )

    def test_later_decline_supersedes_an_escalation_marker(self) -> None:
        threads = [
            {
                "isResolved": False,
                "comments": {
                    "nodes": [
                        {"author": {"login": "reviewer"}, "body": "Finding"},
                        {
                            "author": {"login": "owner"},
                            "authorAssociation": "OWNER",
                            "body": "Escalated without disposition",
                        },
                        {
                            "author": {"login": "owner"},
                            "authorAssociation": "OWNER",
                            "body": "Declined: superseded escalation.",
                        },
                    ]
                },
            }
        ]

        normalized = normalize_review_threads(threads, "owner")

        self.assertFalse(normalized[0]["isEscalated"])
        self.assertEqual(normalized[0]["dispositionKind"], "declined")

    def test_later_non_disposition_reply_ends_an_escalation(self) -> None:
        threads = [
            {
                "isResolved": False,
                "comments": {
                    "nodes": [
                        {"author": {"login": "reviewer"}, "body": "Finding"},
                        {
                            "author": {"login": "owner"},
                            "authorAssociation": "OWNER",
                            "body": "Escalated without disposition",
                        },
                        {
                            "author": {"login": "owner"},
                            "authorAssociation": "OWNER",
                            "body": "Additional context.",
                        },
                    ]
                },
            }
        ]

        normalized = normalize_review_threads(threads, "owner")

        self.assertFalse(normalized[0]["isEscalated"])
        self.assertFalse(normalized[0]["isDispositioned"])
        self.assertIsNone(normalized[0]["dispositionKind"])

    def test_wave_five_escalation_is_eligible_without_extension(self) -> None:
        self._assert_escalation_boundary(wave=5, total_waves=5, eligible=True)

    def test_wave_six_escalation_is_rejected_during_extension(self) -> None:
        self._assert_escalation_boundary(wave=6, total_waves=6, eligible=False)

    def test_wave_eight_escalation_is_eligible_at_hard_stop(self) -> None:
        self._assert_escalation_boundary(wave=8, total_waves=8, eligible=True)

    def test_bot_suffix_reviews_count_toward_escalation_wave(self) -> None:
        self._assert_escalation_boundary(
            wave=5,
            total_waves=5,
            eligible=True,
            reviewer_login="chatgpt-codex-connector[bot]",
        )

    def _assert_escalation_boundary(
        self,
        *,
        wave: int,
        total_waves: int,
        eligible: bool,
        reviewer_login: str = "chatgpt-codex-connector",
    ) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "review_threads": [
                {
                    "dispositionKind": "escalated",
                    "isDispositioned": True,
                    "isEscalated": True,
                    "reviewIds": [f"review-{wave}"],
                }
            ]
        }
        reviews = [
            {
                "id": f"review-{number}",
                "author": {"login": reviewer_login},
                "submittedAt": f"2026-08-16T10:{number:02d}:00Z",
            }
            for number in range(1, total_waves + 1)
        ]

        client._validate_escalation_dispositions(pull_request, reviews)

        thread = pull_request["review_threads"][0]
        self.assertEqual(thread["isDispositioned"], eligible)
        self.assertEqual(thread["isEscalated"], eligible)
        self.assertEqual(
            thread["dispositionKind"], "escalated" if eligible else None
        )

    def test_advanced_base_invalidates_ancestry_comparison(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "node_id": "node",
            "base_oid": "base-old",
            "head_oid": "head",
        }
        comparison = {"behind_by": 0}
        verification = {
            "state0": {"baseRefOid": "base-new", "headRefOid": "head"}
        }
        with (
            mock.patch.object(client, "execute_rest", return_value=comparison),
            mock.patch.object(client, "execute", return_value=verification) as execute,
        ):
            client._load_base_ancestry([pull_request])

        self.assertIsNone(pull_request["base_commits_not_in_head"])
        self.assertEqual(execute.call_count, 1)

    def test_existing_banners_make_modified_file_planning_only(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        pull_request = {
            "base_oid": "base",
            "head_oid": "head",
            "changed_files": [
                {"path": "docs/agents/backlog.md", "changeType": "MODIFIED"}
            ],
        }
        banner = (
            "# Work backlog\n\n"
            "> **Non-authoritative planning scratchpad — do not review for consistency.**\n"
        )
        response = {
            "repository": {
                "head": {"text": banner},
                "base": {"text": banner},
            }
        }
        with mock.patch.object(client, "execute", return_value=response):
            client._load_planning_only_status([pull_request])

        self.assertTrue(pull_request["planning_only"])

    def test_quiet_review_requires_trusted_exact_head_codex_request(self) -> None:
        node = {
            "id": "node",
            "number": 17,
            "state": "OPEN",
            "title": "title",
            "url": "https://example.invalid/pull/17",
            "isDraft": False,
            "author": {"login": "owner"},
            "baseRefName": "main",
            "baseRefOid": "base",
            "headRefName": "agent/work",
            "headRefOid": "head-authenticated",
            "headRepository": {"nameWithOwner": "OWNER/REPOSITORY"},
            "mergeable": "MERGEABLE",
            "reviewThreads": {
                "totalCount": 0,
                "nodes": [],
                "pageInfo": {"hasNextPage": False, "endCursor": None},
            },
            "files": {
                "nodes": [],
                "pageInfo": {"hasNextPage": False, "endCursor": None},
            },
            "comments": {
                "nodes": [
                    {
                        "author": {"login": "owner"},
                        "authorAssociation": "OWNER",
                        "body": "@codex review\nExact head head-authenticated",
                        "createdAt": "2026-08-16T10:00:00Z",
                    }
                ]
            },
            "reviews": {
                "nodes": [
                    {
                        "id": "codex-review",
                        "author": {"login": "chatgpt-codex-connector"},
                        "state": "COMMENTED",
                        "body": "",
                        "submittedAt": "2026-08-16T10:01:00Z",
                        "commit": {"oid": "head-authenticated"},
                        "comments": {"totalCount": 0},
                    },
                    {
                        "id": "human-review",
                        "author": {"login": "human-reviewer"},
                        "state": "COMMENTED",
                        "body": "",
                        "submittedAt": "2026-08-16T10:02:00Z",
                        "commit": {"oid": "unrelated-head"},
                        "comments": {"totalCount": 0},
                    },
                ]
            },
            "commits": {
                "nodes": [
                    {
                        "commit": {
                            "oid": "head-authenticated",
                            "statusCheckRollup": {
                                "state": "SUCCESS",
                                "contexts": {
                                    "nodes": [],
                                    "pageInfo": {
                                        "hasNextPage": False,
                                        "endCursor": None,
                                    },
                                },
                            },
                        }
                    }
                ]
            },
        }

        pull_request = normalize_pull_request(node)
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        client._finalize_review_evidence([pull_request])

        self.assertEqual(
            pull_request["quiet_review_head_oids"],
            ["head-authenticated"],
        )

    def test_graphql_subprocess_timeout_uses_tick_failure_path(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        timeout = subprocess.TimeoutExpired(["gh"], 12)
        with mock.patch("reconcile.subprocess.run", side_effect=timeout) as run:
            with self.assertRaisesRegex(RuntimeError, "GraphQL request timed out"):
                client.execute("query { viewer { login } }", {})
        self.assertEqual(run.call_args.kwargs["timeout"], 12)

    def test_matching_fork_pull_request_is_not_detailed(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        listing = {
            "repository": {
                "pullRequests": {
                    "nodes": [
                        {
                            "id": "fork-node",
                            "headRefName": "agent/untrusted",
                            "headRepository": {"nameWithOwner": "OTHER/FORK"},
                        }
                    ],
                    "pageInfo": {"hasNextPage": False, "endCursor": None},
                }
            },
            "tracked": [],
        }
        with mock.patch.object(client, "execute", return_value=listing) as execute:
            pull_requests, tracked = client.snapshot([], "agent/*")
        self.assertEqual(pull_requests, [])
        self.assertEqual(tracked, [])
        execute.assert_called_once()

    def test_terminal_detail_result_is_recorded_without_processing(self) -> None:
        client = GitHubGraphQL("OWNER/REPOSITORY", 12)
        listing = {
            "repository": {
                "pullRequests": {
                    "nodes": [
                        {
                            "id": "terminal-node",
                            "headRefName": "agent/raced",
                            "headRepository": {"nameWithOwner": "OWNER/REPOSITORY"},
                        }
                    ],
                    "pageInfo": {"hasNextPage": False, "endCursor": None},
                }
            },
            "tracked": [],
        }
        terminal = {
            "nodes": [
                {
                    "id": "terminal-node",
                    "number": 17,
                    "state": "MERGED",
                    "mergedAt": "2026-08-16T00:00:00Z",
                    "closedAt": None,
                    "headRefName": "agent/raced",
                    "headRefOid": "abc123",
                }
            ]
        }
        with mock.patch.object(client, "execute", side_effect=[listing, terminal]):
            pull_requests, tracked = client.snapshot([], "agent/*")
        self.assertEqual(pull_requests, [])
        self.assertEqual(tracked, terminal["nodes"])

    def test_repository_identity_matching_ignores_case(self) -> None:
        client = GitHubGraphQL("owner/repository", 12)
        listing = {
            "repository": {
                "pullRequests": {
                    "nodes": [
                        {
                            "id": "same-repository-node",
                            "headRefName": "agent/work",
                            "headRepository": {"nameWithOwner": "OWNER/REPOSITORY"},
                        }
                    ],
                    "pageInfo": {"hasNextPage": False, "endCursor": None},
                }
            },
            "tracked": [],
        }
        terminal = {
            "nodes": [
                {
                    "id": "same-repository-node",
                    "number": 17,
                    "state": "MERGED",
                    "mergedAt": "2026-08-16T00:00:00Z",
                    "closedAt": None,
                    "headRefName": "agent/work",
                    "headRefOid": "abc123",
                }
            ]
        }
        with mock.patch.object(client, "execute", side_effect=[listing, terminal]) as execute:
            pull_requests, tracked = client.snapshot([], "agent/*")

        self.assertEqual(pull_requests, [])
        self.assertEqual(tracked, terminal["nodes"])
        self.assertEqual(execute.call_count, 2)


class DispatchFenceTests(unittest.TestCase):
    def test_dispatch_fence_uses_time_immediately_before_dispatch(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            state_file = Path(directory) / "state.json"
            config = Config(
                repository="OWNER/REPOSITORY",
                head_pattern="agent/*",
                interval_seconds=300,
                cool_off_seconds=1800,
                command_timeout_seconds=60,
                state_file=state_file,
                log_file=None,
                active_command=("active",),
                dispatch_command=("dispatch",),
                summary="none",
                dry_run=False,
                once=True,
            )
            state = {
                "version": 1,
                "repository": config.repository,
                "pull_requests": {},
            }
            pull_request = {
                "base_commits_not_in_head": 0,
                "node_id": "node",
                "number": 17,
                "title": "title",
                "url": "https://example.invalid/pull/17",
                "is_draft": False,
                "base_ref": "main",
                "head_ref": "agent/work",
                "head_oid": "head",
                "checked_head_oid": "head",
                "check_rollup_state": "SUCCESS",
                "check_inventory": ["CheckRun:required build"],
                "check_inventory_stable": True,
                "mergeable": "MERGEABLE",
                "quiet_review_head_oids": ["head"],
                "review_threads": [
                    {"isResolved": False, "isDispositioned": True}
                ],
                "checks": [],
            }
            inactive = subprocess.CompletedProcess(["active"], 1, "", "")
            accepted = subprocess.CompletedProcess(["dispatch"], 0, "", "")
            logger = mock.Mock()
            with mock.patch(
                "reconcile.run_operator_command", side_effect=[inactive, accepted]
            ), mock.patch("reconcile.time.time", return_value=1300):
                process_pull_request(config, logger, state, pull_request, 1000)

        self.assertEqual(
            state["pull_requests"]["17"]["last_dispatched_at"], 1300
        )
        self.assertEqual(
            state["pull_requests"]["17"]["authenticated_review_head"],
            "head",
        )
        self.assertEqual(
            state["pull_requests"]["17"][
                "authenticated_review_check_inventory"
            ],
            ["CheckRun:required build"],
        )

    def test_state_save_fsyncs_file_and_parent_directory(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            state_file = Path(directory) / "state.json"
            state = {
                "version": 1,
                "repository": "OWNER/REPOSITORY",
                "pull_requests": {},
            }
            with mock.patch("reconcile.os.fsync") as fsync:
                save_state(state_file, state)

        self.assertEqual(fsync.call_count, 2)

    def test_nonzero_dispatch_exit_retains_cool_off_fence(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            state_file = Path(directory) / "state.json"
            config = Config(
                repository="OWNER/REPOSITORY",
                head_pattern="agent/*",
                interval_seconds=300,
                cool_off_seconds=1800,
                command_timeout_seconds=60,
                state_file=state_file,
                log_file=None,
                active_command=("active",),
                dispatch_command=("dispatch",),
                summary="none",
                dry_run=False,
                once=True,
            )
            state = {
                "version": 1,
                "repository": config.repository,
                "pull_requests": {},
            }
            pull_request = {
                "base_commits_not_in_head": 0,
                "node_id": "node",
                "number": 17,
                "title": "title",
                "url": "https://example.invalid/pull/17",
                "is_draft": False,
                "base_ref": "main",
                "head_ref": "agent/work",
                "head_oid": "head",
                "checked_head_oid": "head",
                "check_rollup_state": "SUCCESS",
                "mergeable": "MERGEABLE",
                "quiet_review_head_oids": ["head"],
                "review_threads": [
                    {"isResolved": False, "isDispositioned": True}
                ],
                "checks": [],
            }
            inactive = subprocess.CompletedProcess(["active"], 1, "", "")
            ambiguous = subprocess.CompletedProcess(["dispatch"], 9, "", "")
            logger = mock.Mock()
            with mock.patch(
                "reconcile.run_operator_command", side_effect=[inactive, ambiguous]
            ), mock.patch("reconcile.time.time", return_value=1000):
                result = process_pull_request(config, logger, state, pull_request, 1000)
        self.assertEqual(
            result["reason"], "dispatch-command-exited:9-cool-off-retained"
        )
        self.assertEqual(state["pull_requests"]["17"]["last_dispatched_at"], 1000)


if __name__ == "__main__":
    unittest.main()

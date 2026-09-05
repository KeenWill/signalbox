#!/usr/bin/env python3
"""Keep watched GitHub pull requests moving toward convergence."""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import fnmatch
import io
import json
import math
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import time
import tokenize
from typing import Any, Iterable, Sequence


OPEN_PULL_REQUESTS_QUERY = """
query($owner: String!, $name: String!, $after: String, $tracked: [ID!]!) {
  repository(owner: $owner, name: $name) {
    pullRequests(
      first: 100
      after: $after
      states: OPEN
      orderBy: {field: UPDATED_AT, direction: DESC}
    ) {
      nodes {
        id
        number
        headRefName
        headRepository { nameWithOwner }
      }
      pageInfo { hasNextPage endCursor }
    }
  }
  tracked: nodes(ids: $tracked) {
    ... on PullRequest { id number state mergedAt closedAt headRefName headRefOid }
  }
}
"""

PULL_REQUEST_DETAILS_QUERY = """
query($ids: [ID!]!) {
  nodes(ids: $ids) {
    ... on PullRequest {
      id
      number
      state
      mergedAt
      closedAt
      title
      body
      lastEditedAt
      url
      isDraft
      author { login }
      baseRefName
      baseRefOid
      headRefName
      headRefOid
      headRepository { nameWithOwner }
      mergeable
      reviewThreads(first: 100) {
        nodes {
          id
          isResolved
          comments(first: 100) {
            totalCount
            nodes { author { login } authorAssociation body createdAt lastEditedAt pullRequestReview { id } }
            pageInfo { hasNextPage endCursor }
          }
        }
        pageInfo { hasNextPage endCursor }
      }
      comments(first: 100) {
        nodes { author { login } authorAssociation body createdAt lastEditedAt }
        pageInfo { hasNextPage endCursor }
      }
      reviews(first: 100) {
        nodes {
          id
          author { login }
          state
          submittedAt
          commit { oid }
          comments(first: 1) { totalCount }
        }
        pageInfo { hasNextPage endCursor }
      }
      files(first: 100) {
        nodes { path changeType additions deletions }
        pageInfo { hasNextPage endCursor }
      }
      commits(last: 1) {
        nodes {
          commit {
            oid
            statusCheckRollup {
              state
              contexts(first: 100) {
                nodes {
                  __typename
                  ... on CheckRun { name status conclusion completedAt }
                  ... on StatusContext { context state createdAt }
                }
                pageInfo { hasNextPage endCursor }
              }
            }
          }
        }
      }
    }
  }
}
"""

TERMINAL_PULL_REQUESTS_QUERY = """
query($tracked: [ID!]!) {
  tracked: nodes(ids: $tracked) {
    ... on PullRequest { id number state mergedAt closedAt headRefName headRefOid }
  }
}
"""

GREEN_CHECK_RUN_CONCLUSIONS = frozenset({"SUCCESS", "NEUTRAL", "SKIPPED"})
GREEN_STATUS_CONTEXT_STATES = frozenset({"SUCCESS"})
CODEX_REVIEWER_LOGIN = "chatgpt-codex-connector"
TRUSTED_REVIEW_REQUEST_ASSOCIATIONS = frozenset(
    {"OWNER", "MEMBER", "COLLABORATOR"}
)
PLANNING_ONLY_BANNER = (
    "> **Non-authoritative planning scratchpad — do not review for consistency.**"
)
FIX_DISPOSITION = re.compile(
    r"^fixed in commits?\s+`?([0-9a-f]{7,40})`?", re.IGNORECASE
)
ESCALATION_DISPOSITION = "escalated without disposition"
CODEX_REVIEW_COMMAND = re.compile(r"@codex review(?![\w-])")
EXECUTABLE_COMMENT_DIRECTIVE = re.compile(
    r"^#!|^#.*coding[:=]\s*[-_.a-zA-Z0-9]+"
)
INFORMATIONAL_REVIEW_COMMENT = re.compile(
    r"^(?:question|informational|note)\b", re.IGNORECASE
)
TRIVIAL_INFORMATIONAL_REPLIES = frozenset(
    {"ack", "acknowledged", "done", "noted", "ok", "okay", "thanks", "thank you"}
)
COMPARE_FILE_LIMIT = 300
NON_GATING_CHECK_NAMES = frozenset(
    {
        "codecov/patch",
        "codecov/project",
    }
)
STATE_VERSION = 1
PAGINATION_BATCH_SIZE = 20
DETAIL_BATCH_SIZE = 20


@dataclasses.dataclass(frozen=True)
class Config:
    repository: str
    head_pattern: str
    interval_seconds: float
    cool_off_seconds: float
    command_timeout_seconds: float
    state_file: Path
    log_file: Path | None
    active_command: tuple[str, ...]
    dispatch_command: tuple[str, ...]
    summary: str
    dry_run: bool
    once: bool


@dataclasses.dataclass(frozen=True)
class Decision:
    name: str
    reason: str


@dataclasses.dataclass(frozen=True)
class PaginationTask:
    kind: str
    pull_request: dict[str, Any]
    cursor: str


class GitHubNotFoundError(RuntimeError):
    """A GitHub REST resource was conclusively not found."""


class JsonLogger:
    def __init__(self, path: Path | None) -> None:
        self._stream = path.open("a", encoding="utf-8") if path else sys.stderr

    def write(self, record: dict[str, Any]) -> None:
        print(json.dumps(record, sort_keys=True, separators=(",", ":")), file=self._stream)
        self._stream.flush()

    def close(self) -> None:
        if self._stream is not sys.stderr:
            self._stream.close()


class GitHubGraphQL:
    def __init__(self, repository: str, timeout_seconds: float) -> None:
        self.owner, self.name = split_repository(repository)
        self.repository = repository
        self.timeout_seconds = timeout_seconds

    def execute(self, query: str, variables: dict[str, Any]) -> dict[str, Any]:
        request = json.dumps({"query": query, "variables": variables})
        try:
            completed = subprocess.run(
                ["gh", "api", "graphql", "--input", "-"],
                input=request,
                text=True,
                capture_output=True,
                check=False,
                timeout=self.timeout_seconds,
            )
        except subprocess.TimeoutExpired as error:
            raise RuntimeError("gh GraphQL request timed out") from error
        if completed.returncode != 0:
            detail = completed.stderr.strip() or completed.stdout.strip()
            raise RuntimeError(f"gh GraphQL request failed: {detail}")
        try:
            response = json.loads(completed.stdout)
        except json.JSONDecodeError as error:
            raise RuntimeError("gh GraphQL request returned invalid JSON") from error
        if response.get("errors"):
            raise RuntimeError(f"GitHub GraphQL errors: {json.dumps(response['errors'])}")
        return response["data"]

    def execute_rest(self, path: str) -> Any:
        try:
            completed = subprocess.run(
                ["gh", "api", path],
                text=True,
                capture_output=True,
                check=False,
                timeout=self.timeout_seconds,
            )
        except subprocess.TimeoutExpired as error:
            raise RuntimeError("gh REST request timed out") from error
        if completed.returncode != 0:
            detail = completed.stderr.strip() or completed.stdout.strip()
            if "HTTP 404" in detail:
                raise GitHubNotFoundError(f"gh REST request failed: {detail}")
            raise RuntimeError(f"gh REST request failed: {detail}")
        try:
            return json.loads(completed.stdout)
        except json.JSONDecodeError as error:
            raise RuntimeError("gh REST request returned invalid JSON") from error

    def snapshot(
        self,
        tracked_node_ids: Sequence[str],
        head_pattern: str,
        persisted_records: dict[int, dict[str, Any]] | None = None,
    ) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
        open_pull_requests: list[dict[str, Any]] = []
        tracked: list[dict[str, Any]] = []
        tracked_batches = list(chunks(tracked_node_ids, 100)) or [()]
        first_tracked_batch = tracked_batches.pop(0)
        after: str | None = None
        while True:
            data = self.execute(
                OPEN_PULL_REQUESTS_QUERY,
                {
                    "owner": self.owner,
                    "name": self.name,
                    "after": after,
                    "tracked": list(first_tracked_batch) if after is None else [],
                },
            )
            repository = data.get("repository")
            if repository is None:
                raise RuntimeError("configured GitHub repository is unavailable")
            connection = repository["pullRequests"]
            open_pull_requests.extend(connection["nodes"])
            if after is None:
                tracked.extend(node for node in data["tracked"] if node is not None)
            if not connection["pageInfo"]["hasNextPage"]:
                break
            after = connection["pageInfo"]["endCursor"]
        for tracked_batch in tracked_batches:
            data = self.execute(TERMINAL_PULL_REQUESTS_QUERY, {"tracked": list(tracked_batch)})
            tracked.extend(node for node in data["tracked"] if node is not None)
        tracked_ids = set(tracked_node_ids)
        detail_ids = [
            node["id"]
            for node in open_pull_requests
            if node["id"] in tracked_ids
            or (
                same_repository(head_repository(node), self.repository)
                and fnmatch.fnmatchcase(node.get("headRefName") or "", head_pattern)
            )
        ]
        pull_requests: list[dict[str, Any]] = []
        for detail_batch in chunks(detail_ids, DETAIL_BATCH_SIZE):
            data = self.execute(PULL_REQUEST_DETAILS_QUERY, {"ids": list(detail_batch)})
            for node in data["nodes"]:
                if node is None:
                    continue
                if node["state"] != "OPEN":
                    tracked.append(node)
                    continue
                pull_request = normalize_pull_request(node)
                pull_request["_persisted_record"] = (persisted_records or {}).get(
                    pull_request["number"], {}
                )
                pull_requests.append(pull_request)
        self._finish_paginated_connections(pull_requests)
        self._finish_thread_comments(pull_requests)
        for pull_request in pull_requests:
            resolution_times = pull_request["_persisted_record"].get(
                "resolved_thread_observed_at", {}
            )
            if isinstance(resolution_times, dict):
                for thread in pull_request["review_threads"]:
                    observed_at = resolution_times.get(thread.get("id"))
                    if isinstance(observed_at, str):
                        thread["resolutionObservedAt"] = observed_at
        self._validate_fixing_commits(pull_requests)
        self._finish_review_evidence_connections(pull_requests)
        self._finalize_review_evidence(pull_requests)
        self._restore_persisted_review_evidence(pull_requests)
        self._load_review_exempt_status(pull_requests)
        self._validate_review_waves(pull_requests)
        self._load_renamed_paths(pull_requests)
        self._load_planning_only_status(pull_requests)
        self._load_base_ancestry(pull_requests)
        self._revalidate_checks(pull_requests)
        self._verify_snapshot_oids(pull_requests)
        return pull_requests, tracked

    def _finish_review_evidence_connections(
        self, pull_requests: list[dict[str, Any]]
    ) -> None:
        query = """
query($id: ID!, $commentsAfter: String, $reviewsAfter: String) {
  node(id: $id) {
    ... on PullRequest {
      comments(first: 100, after: $commentsAfter) {
        nodes { author { login } authorAssociation body createdAt lastEditedAt }
        pageInfo { hasNextPage endCursor }
      }
      reviews(first: 100, after: $reviewsAfter) {
        nodes {
          id
          author { login }
          state
          submittedAt
          commit { oid }
          comments(first: 1) { totalCount }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
"""
        for pull_request in pull_requests:
            comment_page = pull_request.pop("_review_comment_page")
            review_page = pull_request.pop("_review_page")
            while comment_page["hasNextPage"] or review_page["hasNextPage"]:
                data = self.execute(
                    query,
                    {
                        "id": pull_request["node_id"],
                        "commentsAfter": (
                            comment_page["endCursor"]
                            if comment_page["hasNextPage"]
                            else None
                        ),
                        "reviewsAfter": (
                            review_page["endCursor"]
                            if review_page["hasNextPage"]
                            else None
                        ),
                    },
                )
                node = data.get("node")
                if node is None:
                    raise RuntimeError("pull request became unavailable")
                if comment_page["hasNextPage"]:
                    comments = node["comments"]
                    pull_request["_review_comments"].extend(comments["nodes"])
                    comment_page = comments["pageInfo"]
                if review_page["hasNextPage"]:
                    reviews = node["reviews"]
                    pull_request["_reviews"].extend(reviews["nodes"])
                    review_page = reviews["pageInfo"]

    def _finish_thread_comments(
        self, pull_requests: list[dict[str, Any]]
    ) -> None:
        query = """
query($id: ID!, $after: String!) {
  node(id: $id) {
    ... on PullRequestReviewThread {
      comments(first: 100, after: $after) {
        nodes { author { login } authorAssociation body createdAt lastEditedAt pullRequestReview { id } }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
"""
        for pull_request in pull_requests:
            for thread in pull_request.pop("_review_thread_nodes"):
                comments = thread["comments"]
                page = comments["pageInfo"]
                while page["hasNextPage"]:
                    data = self.execute(
                        query, {"id": thread["id"], "after": page["endCursor"]}
                    )
                    node = data.get("node")
                    if node is None:
                        raise RuntimeError("review thread became unavailable")
                    next_comments = node["comments"]
                    comments["nodes"].extend(next_comments["nodes"])
                    page = next_comments["pageInfo"]
                pull_request["review_threads"].extend(
                    normalize_review_threads([thread], pull_request["author_login"])
                )

    def _validate_fixing_commits(
        self, pull_requests: list[dict[str, Any]]
    ) -> None:
        for pull_request in pull_requests:
            validity: dict[str, bool] = {}
            for thread in pull_request["review_threads"]:
                fixing_commit = thread.get("fixingCommit")
                if thread.get("dispositionKind") != "fixed":
                    continue
                if not isinstance(fixing_commit, str):
                    thread["isDispositioned"] = False
                    thread["dispositionKind"] = None
                    continue
                if fixing_commit not in validity:
                    try:
                        comparison = self.execute_rest(
                            f"repos/{self.owner}/{self.name}/compare/"
                            f"{fixing_commit}...{pull_request['head_oid']}"
                        )
                    except GitHubNotFoundError:
                        # A nonexistent or mistyped commit hash in a "Fixed in
                        # commit ..." reply is an invalid disposition, not a
                        # tick-aborting failure: mark it undispositioned like
                        # any other unverifiable fixing commit.
                        validity[fixing_commit] = False
                    else:
                        base_commit = comparison.get("base_commit")
                        head_commit = comparison.get("head_commit")
                        merge_base = comparison.get("merge_base_commit")
                        validity[fixing_commit] = (
                            comparison.get("status") in {"ahead", "identical"}
                            and isinstance(base_commit, dict)
                            and isinstance(head_commit, dict)
                            and isinstance(merge_base, dict)
                            and base_commit.get("sha") == merge_base.get("sha")
                            and head_commit.get("sha") == pull_request["head_oid"]
                        )
                if not validity[fixing_commit]:
                    thread["isDispositioned"] = False
                    thread["dispositionKind"] = None
                    thread["fixingCommit"] = None

    def _finalize_review_evidence(
        self, pull_requests: list[dict[str, Any]]
    ) -> None:
        for pull_request in pull_requests:
            comments = pull_request.pop("_review_comments")
            reviews = pull_request.pop("_reviews")
            quiet_oids: list[str] = []
            observed_codex_reviews: dict[str, str] = {}
            live_codex_review_oids: dict[str, str] = {}
            authenticated_review_ids: dict[str, str] = {}
            for review in reviews:
                commit = review.get("commit")
                reviewed_oid = commit.get("oid") if isinstance(commit, dict) else None
                submitted_at = review.get("submittedAt")
                if (
                    reviewed_oid is None
                    or submitted_at is None
                    or review.get("state") == "DISMISSED"
                    or (
                        isinstance(pull_request.get("body_last_edited_at"), str)
                        and pull_request["body_last_edited_at"] > submitted_at
                    )
                ):
                    continue
                request_times = [
                    effective_at
                    for comment in comments
                    for effective_at in [comment_effective_at(comment)]
                    if is_codex_review_request(comment, reviewed_oid)
                    and effective_at is not None
                    and effective_at <= submitted_at
                    and checks_green_before_request(
                        pull_request["checks"],
                        pull_request["check_rollup_state"],
                        effective_at,
                    )
                    and prior_threads_dispositioned_before(
                        pull_request.get("review_threads", []), effective_at
                    )
                ]
                review_id = review.get("id")
                is_live_codex_review = (
                    author_login(review) is not None
                    and author_login(review).casefold()
                    == CODEX_REVIEWER_LOGIN.casefold()
                    and isinstance(review_id, str)
                )
                if is_live_codex_review:
                    # Tracks every live (non-dismissed, not stale-body) Codex
                    # review regardless of whether a qualifying request was
                    # found this tick, so a previously authenticated review
                    # can be reconfirmed even when gating checks were rerun
                    # (and so now postdate the original request) on the same,
                    # unchanged head.
                    live_codex_review_oids[review_id] = reviewed_oid
                if is_live_codex_review and request_times:
                    observed_codex_reviews[review_id] = reviewed_oid
                review_threads = [
                    thread
                    for thread in pull_request.get("review_threads", [])
                    if review_id in thread.get("reviewIds", [])
                ]
                all_findings_declined = bool(review_threads) and all(
                    thread["isResolved"]
                    and thread.get("dispositionKind") == "declined"
                    for thread in review_threads
                )
                informational_wave = bool(review_threads) and all(
                    thread["isResolved"]
                    and thread["isDispositioned"]
                    and thread.get("isInformational", False)
                    for thread in review_threads
                )
                if (
                    request_times
                    and review.get("state") != "CHANGES_REQUESTED"
                    and author_login(review) is not None
                    and author_login(review).casefold()
                    == CODEX_REVIEWER_LOGIN.casefold()
                    and (
                        review["comments"]["totalCount"] == 0
                        or all_findings_declined
                        or informational_wave
                    )
                ):
                    quiet_oids.append(reviewed_oid)
                    if isinstance(review_id, str):
                        authenticated_review_ids[reviewed_oid] = review_id
            pull_request["authenticated_quiet_review_oids"] = quiet_oids
            pull_request["authenticated_review_ids"] = authenticated_review_ids
            pull_request["observed_codex_reviews"] = observed_codex_reviews
            pull_request["live_codex_review_oids"] = live_codex_review_oids
            pull_request["_codex_reviews"] = [
                review
                for review in reviews
                if isinstance(review.get("id"), str)
                and review["id"] in observed_codex_reviews
            ]
            pull_request["quiet_review_head_oids"] = [
                oid for oid in quiet_oids if oid == pull_request["head_oid"]
            ]

    def _restore_persisted_review_evidence(
        self, pull_requests: list[dict[str, Any]]
    ) -> None:
        for pull_request in pull_requests:
            record = pull_request["_persisted_record"]
            persisted_head = record.get("authenticated_review_head")
            persisted_review_id = record.get("authenticated_review_id")
            live_codex_review_oids = pull_request.pop("live_codex_review_oids", {})
            gating_checks = [
                check
                for check in pull_request.get("checks", [])
                if not is_non_gating_check(check)
            ]
            checks_currently_green = pull_request.get(
                "check_rollup_state"
            ) is not None and all(
                check_is_green(check) for check in gating_checks
            )
            # A rerun of gating checks on the same, unchanged head advances
            # those checks' completion timestamps, so the fresh recomputation
            # in `_finalize_review_evidence` can stop finding a qualifying
            # request for an already-authenticated review even though nothing
            # meaningful changed. Reconfirm the persisted review directly
            # against the live (non-dismissed) review set and the checks'
            # current state rather than losing the evidence outright.
            review_still_valid = isinstance(persisted_review_id, str) and (
                record.get("authenticated_review_body") == pull_request["body"]
            ) and (
                pull_request["observed_codex_reviews"].get(persisted_review_id)
                == persisted_head
                or (
                    persisted_head == pull_request["head_oid"]
                    and live_codex_review_oids.get(persisted_review_id)
                    == persisted_head
                    and checks_currently_green
                )
            )
            if (
                review_still_valid
                and persisted_head
                not in pull_request["authenticated_quiet_review_oids"]
            ):
                pull_request["authenticated_quiet_review_oids"].append(
                    persisted_head
                )
                pull_request["authenticated_review_ids"][persisted_head] = (
                    persisted_review_id
                )
            if (
                review_still_valid
                and persisted_head == pull_request["head_oid"]
                and persisted_head not in pull_request["quiet_review_head_oids"]
            ):
                pull_request["quiet_review_head_oids"].append(persisted_head)
            pull_request.pop("observed_codex_reviews", None)

    def _validate_review_waves(
        self, pull_requests: list[dict[str, Any]]
    ) -> None:
        for pull_request in pull_requests:
            record = pull_request.pop("_persisted_record")
            reviews = pull_request.pop("_codex_reviews")
            current_ids = [review["id"] for review in reviews]
            known_ids = [
                review_id
                for review_id in record.get("known_codex_review_ids", [])
                if isinstance(review_id, str)
            ]
            wave_ids = [
                review_id
                for review_id in record.get("review_wave_ids", [])
                if isinstance(review_id, str)
            ]
            if not known_ids and not wave_ids:
                wave_ids = list(current_ids)
            new_ids = [
                review_id for review_id in current_ids if review_id not in known_ids
            ]
            prior_base = record.get("review_wave_base_oid")
            persisted_head = record.get("head_oid")
            material_base_forward = False
            if (
                isinstance(prior_base, str)
                and prior_base != pull_request["base_oid"]
                and isinstance(persisted_head, str)
                and persisted_head != pull_request["head_oid"]
            ):
                material_base_forward = not self._review_exempt_change(
                    persisted_head,
                    pull_request["head_oid"],
                    pull_request["base_oid"],
                )
            if material_base_forward:
                wave_ids = list(new_ids)
            else:
                wave_ids.extend(
                    review_id
                    for review_id in new_ids
                    if review_id not in wave_ids
                )
            pull_request["known_codex_review_ids"] = current_ids
            pull_request["review_wave_ids"] = wave_ids
            wave_reviews = [
                review for review in reviews if review["id"] in wave_ids
            ]
            self._validate_escalation_dispositions(pull_request, wave_reviews)

    def _validate_escalation_dispositions(
        self, pull_request: dict[str, Any], reviews: Sequence[dict[str, Any]]
    ) -> None:
        codex_reviews = sorted(
            (
                review
                for review in reviews
                if author_login(review) is not None
                and author_login(review).casefold()
                == CODEX_REVIEWER_LOGIN.casefold()
                and review.get("state") != "DISMISSED"
                and isinstance(review.get("submittedAt"), str)
            ),
            key=lambda review: review["submittedAt"],
        )
        wave_by_review_id = {
            review["id"]: wave
            for wave, review in enumerate(codex_reviews, start=1)
            if isinstance(review.get("id"), str)
        }
        total_waves = len(codex_reviews)
        for thread in pull_request.get("review_threads", []):
            if thread.get("dispositionKind") != "escalated":
                continue
            waves = [
                wave_by_review_id[review_id]
                for review_id in thread.get("reviewIds", [])
                if review_id in wave_by_review_id
            ]
            wave = max(waves, default=0)
            # AGENTS.md permits this marker at the ordinary wave-five stop
            # only when no extension was taken, or at the wave-eight hard stop.
            eligible = wave >= 8 or (wave == 5 and total_waves == 5)
            if not eligible:
                thread["isDispositioned"] = False
                thread["isEscalated"] = False
                thread["dispositionKind"] = None

    def _load_review_exempt_status(
        self, pull_requests: list[dict[str, Any]]
    ) -> None:
        for pull_request in pull_requests:
            pull_request["review_exempt_since_quiet_review"] = False
            quiet_oids = pull_request.pop("authenticated_quiet_review_oids")
            if pull_request["quiet_review_head_oids"]:
                continue
            for reviewed_oid in reversed(quiet_oids):
                try:
                    exempt = self._review_exempt_change(
                        reviewed_oid,
                        pull_request["head_oid"],
                        pull_request["base_oid"],
                    )
                except GitHubNotFoundError:
                    continue
                if exempt:
                    pull_request["review_exempt_since_quiet_review"] = True
                    break

    def _review_exempt_change(
        self, reviewed_oid: str, head_oid: str, base_oid: str
    ) -> bool:
        comparison = self.execute_rest(
            f"repos/{self.owner}/{self.name}/compare/{reviewed_oid}...{head_oid}"
        )
        commits = comparison.get("commits") if isinstance(comparison, dict) else None
        files = comparison.get("files") if isinstance(comparison, dict) else None
        if not comparison_is_complete(comparison) or not commits:
            return False
        if files and all(
            file.get("status") == "renamed"
            and file.get("changes") == 0
            and file.get("additions") == 0
            and file.get("deletions") == 0
            for file in files
        ):
            return True
        if self._is_clean_merge_forward(
            comparison, reviewed_oid, head_oid, base_oid
        ):
            return True
        return bool(files) and all(comment_only_patch(file) for file in files)

    def _is_clean_merge_forward(
        self,
        comparison: dict[str, Any],
        reviewed_oid: str,
        head_oid: str,
        base_oid: str,
    ) -> bool:
        commits = comparison.get("commits")
        files = comparison.get("files")
        if not isinstance(commits, list) or len(commits) != 1:
            return False
        merge_commit = commits[0]
        parents = merge_commit.get("parents")
        if (
            merge_commit.get("sha") != head_oid
            or not isinstance(parents, list)
            or [parent.get("sha") for parent in parents]
            != [reviewed_oid, base_oid]
        ):
            return False
        head_delta = comparison_file_delta(files)
        if head_delta is None:
            return False
        reviewed_to_base = self.execute_rest(
            f"repos/{self.owner}/{self.name}/compare/{reviewed_oid}...{base_oid}"
        )
        merge_base = (
            reviewed_to_base.get("merge_base_commit")
            if isinstance(reviewed_to_base, dict)
            else None
        )
        merge_base_oid = (
            merge_base.get("sha") if isinstance(merge_base, dict) else None
        )
        if not isinstance(merge_base_oid, str):
            return False
        base_comparison = self.execute_rest(
            f"repos/{self.owner}/{self.name}/compare/{merge_base_oid}...{base_oid}"
        )
        if not comparison_is_complete(base_comparison):
            return False
        base_files = (
            base_comparison.get("files")
            if isinstance(base_comparison, dict)
            else None
        )
        base_delta = comparison_file_delta(base_files)
        return base_delta is not None and head_delta == base_delta

    def _load_renamed_paths(
        self, pull_requests: list[dict[str, Any]]
    ) -> None:
        for pull_request in pull_requests:
            renamed = {
                changed_file["path"]: changed_file
                for changed_file in pull_request["changed_files"]
                if changed_file["changeType"] == "RENAMED"
            }
            page = 1
            while renamed:
                files = self.execute_rest(
                    f"repos/{self.owner}/{self.name}/pulls/{pull_request['number']}/files"
                    f"?per_page=100&page={page}"
                )
                if not isinstance(files, list):
                    raise RuntimeError("GitHub pull-request files response is malformed")
                for changed_file in files:
                    current = renamed.get(changed_file.get("filename"))
                    previous = changed_file.get("previous_filename")
                    if current is not None and isinstance(previous, str):
                        current["previous_path"] = previous
                        del renamed[current["path"]]
                if len(files) < 100:
                    break
                page += 1
            if renamed:
                raise RuntimeError("renamed pull-request file lacks its base path")

    def _load_base_ancestry(
        self, pull_requests: list[dict[str, Any]]
    ) -> None:
        comparisons: list[tuple[dict[str, Any], int | None]] = []
        for offset in range(0, len(pull_requests), DETAIL_BATCH_SIZE):
            batch = pull_requests[offset : offset + DETAIL_BATCH_SIZE]
            declarations = ["$owner: String!", "$name: String!"]
            selections: list[str] = []
            variables: dict[str, Any] = {
                "owner": self.owner,
                "name": self.name,
            }
            for index, pull_request in enumerate(batch):
                variable = f"basehead{index}"
                declarations.append(f"${variable}: String!")
                variables[variable] = (
                    f"{pull_request['base_oid']}...{pull_request['head_oid']}"
                )
                selections.append(
                    f"item{index}: comparison(basehead: ${variable}) {{ behindBy }}"
                )
            query = (
                f"query({', '.join(declarations)}) {{ "
                f"repository(owner: $owner, name: $name) "
                f"{{ {' '.join(selections)} }} }}"
            )
            data = self.execute(query, variables)
            repository = data.get("repository")
            if repository is None:
                raise RuntimeError("configured GitHub repository is unavailable")
            for index, pull_request in enumerate(batch):
                comparison = repository[f"item{index}"]
                comparisons.append(
                    (
                        pull_request,
                        comparison["behindBy"] if comparison is not None else None,
                    )
                )

        for offset in range(0, len(comparisons), DETAIL_BATCH_SIZE):
            batch = comparisons[offset : offset + DETAIL_BATCH_SIZE]
            declarations: list[str] = []
            selections: list[str] = []
            variables: dict[str, Any] = {}
            for index, (pull_request, _) in enumerate(batch):
                variable = f"node{index}"
                declarations.append(f"${variable}: ID!")
                variables[variable] = pull_request["node_id"]
                selections.append(
                    f"state{index}: node(id: ${variable}) {{ ... on PullRequest "
                    "{ baseRefOid headRefOid } }"
                )
            verification = self.execute(
                f"query({', '.join(declarations)}) {{ {' '.join(selections)} }}",
                variables,
            )
            for index, (pull_request, behind_by) in enumerate(batch):
                current = verification[f"state{index}"]
                snapshot_still_current = (
                    current is not None
                    and current["baseRefOid"] == pull_request["base_oid"]
                    and current["headRefOid"] == pull_request["head_oid"]
                )
                pull_request["base_commits_not_in_head"] = (
                    behind_by if snapshot_still_current else None
                )

    def _revalidate_checks(
        self, pull_requests: list[dict[str, Any]]
    ) -> None:
        query = """
query($id: ID!) {
  node(id: $id) {
    ... on PullRequest {
      baseRefOid
      headRefOid
      commits(last: 1) {
        nodes {
          commit {
            oid
            statusCheckRollup {
              state
              contexts(first: 100) {
                nodes {
                  __typename
                  ... on CheckRun { name status conclusion completedAt }
                  ... on StatusContext { context state createdAt }
                }
                pageInfo { hasNextPage endCursor }
              }
            }
          }
        }
      }
    }
  }
}
"""
        for pull_request in pull_requests:
            data = self.execute(query, {"id": pull_request["node_id"]})
            node = data.get("node")
            if node is None:
                raise RuntimeError("pull request became unavailable during check revalidation")
            if (
                node["baseRefOid"] != pull_request["base_oid"]
                or node["headRefOid"] != pull_request["head_oid"]
            ):
                pull_request["checked_head_oid"] = None
                pull_request["base_commits_not_in_head"] = None
                continue
            commit_nodes = node["commits"]["nodes"]
            commit = commit_nodes[0]["commit"] if commit_nodes else None
            rollup = commit.get("statusCheckRollup") if commit else None
            contexts = (
                rollup["contexts"]
                if rollup
                else {"nodes": [], "pageInfo": empty_page_info()}
            )
            pull_request["checked_head_oid"] = commit.get("oid") if commit else None
            pull_request["check_rollup_state"] = (
                rollup.get("state") if rollup else None
            )
            pull_request["checks"] = list(contexts["nodes"])
            page = contexts["pageInfo"]
            while page["hasNextPage"]:
                task = PaginationTask("checks", pull_request, page["endCursor"])
                page_query, variables = pagination_query([task])
                page_data = self.execute(page_query, variables)
                page_node = page_data.get("item0")
                if page_node is None:
                    raise RuntimeError(
                        "pull request became unavailable during check revalidation"
                    )
                next_page = pagination_page(page_node, "checks")
                pull_request["checks"].extend(next_page["nodes"])
                page = next_page["pageInfo"]

    def _verify_snapshot_oids(
        self,
        pull_requests: list[dict[str, Any]],
        *,
        raise_on_change: bool = False,
    ) -> None:
        for offset in range(0, len(pull_requests), DETAIL_BATCH_SIZE):
            batch = pull_requests[offset : offset + DETAIL_BATCH_SIZE]
            declarations: list[str] = []
            selections: list[str] = []
            variables: dict[str, Any] = {}
            for index, pull_request in enumerate(batch):
                variable = f"node{index}"
                declarations.append(f"${variable}: ID!")
                variables[variable] = pull_request["node_id"]
                selections.append(
                    f"item{index}: node(id: ${variable}) {{ ... on PullRequest "
                    "{ baseRefOid headRefOid } }"
                )
            data = self.execute(
                f"query({', '.join(declarations)}) {{ {' '.join(selections)} }}",
                variables,
            )
            for index, pull_request in enumerate(batch):
                node = data.get(f"item{index}")
                if node is None:
                    raise RuntimeError("pull request became unavailable during revalidation")
                if (
                    node["baseRefOid"] != pull_request["base_oid"]
                    or node["headRefOid"] != pull_request["head_oid"]
                ):
                    if raise_on_change:
                        raise RuntimeError(
                            "pull request changed after its convergence snapshot"
                        )
                    pull_request["checked_head_oid"] = None
                    pull_request["base_commits_not_in_head"] = None

    def _load_planning_only_status(
        self, pull_requests: list[dict[str, Any]]
    ) -> None:
        for pull_request in pull_requests:
            changed_files = pull_request["changed_files"]
            if not changed_files:
                pull_request["planning_only"] = False
                continue
            planning_only = True
            for changed_file in changed_files:
                base_path = changed_file.get("previous_path") or changed_file["path"]
                data = self.execute(
                    """
query($owner: String!, $name: String!, $head: String!, $base: String!) {
  repository(owner: $owner, name: $name) {
    head: object(expression: $head) { ... on Blob { text } }
    base: object(expression: $base) { ... on Blob { text } }
  }
}
""",
                    {
                        "owner": self.owner,
                        "name": self.name,
                        "head": f"{pull_request['head_oid']}:{changed_file['path']}",
                        "base": f"{pull_request['base_oid']}:{base_path}",
                    },
                )
                repository = data.get("repository")
                if repository is None:
                    raise RuntimeError("configured GitHub repository is unavailable")
                head_has_banner = blob_has_planning_banner(repository["head"])
                base_has_banner = blob_has_planning_banner(repository["base"])
                eligible = head_has_banner and (
                    changed_file["changeType"] == "ADDED" or base_has_banner
                )
                if not eligible:
                    planning_only = False
                    break
            pull_request["planning_only"] = planning_only

    def _finish_paginated_connections(self, pull_requests: list[dict[str, Any]]) -> None:
        pending: list[PaginationTask] = []
        for pull_request in pull_requests:
            thread_page = pull_request.pop("_thread_page")
            check_page = pull_request.pop("_check_page")
            file_page = pull_request.pop("_file_page")
            if thread_page["hasNextPage"]:
                pending.append(PaginationTask("threads", pull_request, thread_page["endCursor"]))
            if check_page["hasNextPage"]:
                pending.append(PaginationTask("checks", pull_request, check_page["endCursor"]))
            if file_page["hasNextPage"]:
                pending.append(PaginationTask("files", pull_request, file_page["endCursor"]))
        while pending:
            batch = pending[:PAGINATION_BATCH_SIZE]
            del pending[:PAGINATION_BATCH_SIZE]
            query, variables = pagination_query(batch)
            data = self.execute(query, variables)
            for index, task in enumerate(batch):
                node = data.get(f"item{index}")
                if node is None:
                    raise RuntimeError(
                        "pull request became unavailable during pagination"
                    )
                page = pagination_page(node, task.kind)
                if task.kind == "threads":
                    task.pull_request["_review_thread_nodes"].extend(page["nodes"])
                elif task.kind == "checks":
                    task.pull_request["checks"].extend(page["nodes"])
                else:
                    task.pull_request["changed_files"].extend(page["nodes"])
                if page["pageInfo"]["hasNextPage"]:
                    pending.append(
                        PaginationTask(task.kind, task.pull_request, page["pageInfo"]["endCursor"])
                    )


def chunks(values: Sequence[str], size: int) -> Iterable[Sequence[str]]:
    for offset in range(0, len(values), size):
        yield values[offset : offset + size]


def split_repository(repository: str) -> tuple[str, str]:
    parts = repository.split("/")
    if len(parts) != 2 or not all(parts):
        raise ValueError("repository must have the form OWNER/NAME")
    return parts[0], parts[1]


def head_repository(node: dict[str, Any]) -> str | None:
    repository = node.get("headRepository")
    return repository.get("nameWithOwner") if isinstance(repository, dict) else None


def same_repository(left: str | None, right: str) -> bool:
    return left is not None and left.casefold() == right.casefold()


def author_login(node: dict[str, Any]) -> str | None:
    author = node.get("author")
    return author.get("login") if isinstance(author, dict) else None


def blob_has_planning_banner(blob: dict[str, Any] | None) -> bool:
    if not isinstance(blob, dict) or not isinstance(blob.get("text"), str):
        return False
    return PLANNING_ONLY_BANNER in blob["text"].splitlines()[:10]


def fixing_commit(body: str) -> str | None:
    match = FIX_DISPOSITION.match(body.strip())
    return match.group(1) if match else None


def disposition_kind(body: str) -> str | None:
    stripped = body.strip()
    if fixing_commit(stripped) is not None:
        return "fixed"
    if stripped.casefold().startswith("declined:") and stripped[9:].strip():
        return "declined"
    if stripped.casefold() == ESCALATION_DISPOSITION:
        return "escalated"
    return None


def comment_effective_at(comment: dict[str, Any]) -> str | None:
    created_at = comment.get("createdAt")
    edited_at = comment.get("lastEditedAt")
    timestamps = [
        value for value in (created_at, edited_at) if isinstance(value, str)
    ]
    return max(timestamps) if timestamps else None


def is_codex_review_request(comment: dict[str, Any], head_oid: str) -> bool:
    body = comment.get("body") or ""
    requests_review = any(
        CODEX_REVIEW_COMMAND.match(line.strip().casefold())
        for line in body.splitlines()
    )
    return (
        comment.get("authorAssociation")
        in TRUSTED_REVIEW_REQUEST_ASSOCIATIONS
        and requests_review
        and head_oid.casefold() in body.casefold()
    )


def prior_threads_dispositioned_before(
    threads: Sequence[dict[str, Any]], requested_at: str
) -> bool:
    for thread in threads:
        reviewer_at = thread.get("latestReviewerAt")
        if not isinstance(reviewer_at, str) or reviewer_at >= requested_at:
            continue
        disposition_at = thread.get("dispositionAt")
        resolution_observed_at = thread.get("resolutionObservedAt")
        if (
            not thread["isResolved"]
            or not thread["isDispositioned"]
            or not isinstance(disposition_at, str)
            or disposition_at > requested_at
            or not isinstance(resolution_observed_at, str)
            or resolution_observed_at > requested_at
        ):
            return False
    return True


def checks_green_before_request(
    checks: Sequence[dict[str, Any]],
    rollup_state: str | None,
    requested_at: str,
) -> bool:
    if rollup_state is None:
        return False
    gating_checks = [check for check in checks if not is_non_gating_check(check)]
    for check in gating_checks:
        observed_at = (
            check.get("completedAt")
            if check["__typename"] == "CheckRun"
            else check.get("createdAt")
        )
        if (
            not check_is_green(check)
            or not isinstance(observed_at, str)
            or observed_at > requested_at
        ):
            return False
    return True


def comment_only_patch(file: dict[str, Any]) -> bool:
    filename = file.get("filename")
    patch = file.get("patch")
    if not isinstance(filename, str) or not isinstance(patch, str):
        return False
    if Path(filename).suffix.casefold() != ".py":
        return False

    def side_is_comment_only(prefix: str) -> tuple[bool, bool]:
        source: list[str] = []
        changed_rows: set[int] = set()
        saw_change = False
        inside_hunk = False
        for line in patch.splitlines():
            if line.startswith("@@"):
                inside_hunk = True
                source.append("\n")
                continue
            if not inside_hunk or not line:
                continue
            marker = line[0]
            if marker == " ":
                source.append(line[1:] + "\n")
            elif marker == prefix:
                source.append(line[1:] + "\n")
                changed_rows.add(len(source))
                saw_change = True
        if not saw_change:
            return True, False
        try:
            tokens = tokenize.generate_tokens(io.StringIO("".join(source)).readline)
            comment_rows = {
                token.start[0] for token in tokens if token.type == tokenize.COMMENT
            }
        except (IndentationError, SyntaxError, tokenize.TokenError):
            return False, True
        meaningful_rows = {
            row for row in changed_rows if source[row - 1].strip()
        }
        # Shebangs and PEP 263 encoding cookies tokenize as comments but change
        # runtime behaviour, so they never qualify for the comment-only
        # exemption. Patch hunks carry no absolute line numbers, so any changed
        # row that looks like such a directive is rejected conservatively.
        if any(
            EXECUTABLE_COMMENT_DIRECTIVE.match(source[row - 1].strip())
            for row in meaningful_rows
        ):
            return False, True
        return meaningful_rows <= comment_rows, True

    added_ok, added = side_is_comment_only("+")
    removed_ok, removed = side_is_comment_only("-")
    return (added or removed) and added_ok and removed_ok


def comparison_file_delta(files: Any) -> tuple[tuple[Any, ...], ...] | None:
    if not isinstance(files, list):
        return None
    normalized: list[tuple[Any, ...]] = []
    for changed_file in files:
        if not isinstance(changed_file, dict):
            return None
        filename = changed_file.get("filename")
        status = changed_file.get("status")
        additions = changed_file.get("additions")
        deletions = changed_file.get("deletions")
        changes = changed_file.get("changes")
        patch = changed_file.get("patch")
        if (
            not isinstance(filename, str)
            or not isinstance(status, str)
            or not isinstance(additions, int)
            or not isinstance(deletions, int)
            or not isinstance(changes, int)
            or (changes > 0 and not isinstance(patch, str))
        ):
            return None
        normalized.append(
            (
                filename,
                changed_file.get("previous_filename"),
                status,
                additions,
                deletions,
                changes,
                changed_file.get("sha"),
                patch,
            )
        )
    return tuple(sorted(normalized))


def comparison_is_complete(comparison: Any) -> bool:
    if not isinstance(comparison, dict):
        return False
    commits = comparison.get("commits")
    files = comparison.get("files")
    total_commits = comparison.get("total_commits")
    return (
        isinstance(commits, list)
        and isinstance(files, list)
        and isinstance(total_commits, int)
        and total_commits == len(commits)
        and len(files) < COMPARE_FILE_LIMIT
    )


def normalize_review_threads(
    threads: Sequence[dict[str, Any]], pull_request_author: str | None
) -> list[dict[str, Any]]:
    normalized: list[dict[str, Any]] = []
    for thread in threads:
        comments = thread["comments"]["nodes"]
        latest_reviewer_index = max(
            (
                index
                for index, comment in enumerate(comments)
                if index == 0
                or isinstance(comment.get("pullRequestReview"), dict)
                or comment.get("authorAssociation")
                not in TRUSTED_REVIEW_REQUEST_ASSOCIATIONS
            ),
            default=0,
        )
        author_replies = [
            comment
            for comment in comments[latest_reviewer_index + 1 :]
            if comment.get("authorAssociation")
            in TRUSTED_REVIEW_REQUEST_ASSOCIATIONS
        ]
        dispositions = [
            disposition_kind(comment.get("body") or "")
            for comment in author_replies
        ]
        latest_disposition = next(
            (kind for kind in reversed(dispositions) if kind is not None),
            None,
        )
        fixing_commits = [
            fixing_commit(comment.get("body") or "")
            for comment in author_replies
        ]
        latest_reviewer_at = max(
            (
                comment["createdAt"]
                for comment in comments[: latest_reviewer_index + 1]
                if isinstance(comment.get("createdAt"), str)
            ),
            default=None,
        )
        disposition_at = max(
            (
                effective_at
                for comment, kind in zip(author_replies, dispositions)
                for effective_at in [comment_effective_at(comment)]
                if kind is not None and effective_at is not None
            ),
            default=None,
        )
        review_ids = sorted(
            {
                review["id"]
                for comment in comments
                for review in [comment.get("pullRequestReview")]
                if isinstance(review, dict) and isinstance(review.get("id"), str)
            }
        )
        first_body = (comments[0].get("body") or "") if comments else ""
        informational = INFORMATIONAL_REVIEW_COMMENT.search(first_body.strip()) is not None
        informational_answers = [
            comment
            for comment in author_replies
            if (comment.get("body") or "").strip()
            and (comment.get("body") or "").strip().casefold()
            not in TRIVIAL_INFORMATIONAL_REPLIES
        ]
        dispositioned = (
            bool(informational_answers)
            if informational
            else any(kind is not None for kind in dispositions)
        )
        if informational and informational_answers:
            disposition_at = max(
                (
                    effective_at
                    for comment in informational_answers
                    for effective_at in [comment_effective_at(comment)]
                    if effective_at is not None
                ),
                default=None,
            )
        normalized_thread = {
            "isResolved": thread["isResolved"],
            "isDispositioned": dispositioned,
            "isEscalated": latest_disposition == "escalated",
            "isInformational": informational,
            "latestReviewerAt": latest_reviewer_at,
            "dispositionAt": disposition_at,
            "dispositionKind": latest_disposition,
            "fixingCommit": next(
                (commit for commit in reversed(fixing_commits) if commit is not None),
                None,
            ),
            "reviewIds": review_ids,
        }
        thread_id = thread.get("id")
        if isinstance(thread_id, str):
            normalized_thread["id"] = thread_id
        normalized.append(normalized_thread)
    return normalized


def normalize_pull_request(node: dict[str, Any]) -> dict[str, Any]:
    commit_nodes = node["commits"]["nodes"]
    commit = commit_nodes[0]["commit"] if commit_nodes else None
    rollup = commit.get("statusCheckRollup") if commit else None
    contexts = rollup["contexts"] if rollup else {"nodes": [], "pageInfo": empty_page_info()}
    threads = node["reviewThreads"]
    files = node["files"]
    pull_request_author = author_login(node)
    return {
        "node_id": node["id"],
        "number": node["number"],
        "state": node["state"],
        "title": node["title"],
        "body": node.get("body") or "",
        "body_last_edited_at": node.get("lastEditedAt"),
        "url": node["url"],
        "is_draft": node["isDraft"],
        "author_login": pull_request_author,
        "base_ref": node["baseRefName"],
        "base_oid": node["baseRefOid"],
        "head_ref": node["headRefName"],
        "head_oid": node["headRefOid"],
        "head_repository": head_repository(node),
        "mergeable": node["mergeable"],
        "checked_head_oid": commit["oid"] if commit else None,
        "review_threads": [],
        "quiet_review_head_oids": [],
        "check_rollup_state": rollup.get("state") if rollup else None,
        "checks": list(contexts["nodes"]),
        "changed_files": list(files["nodes"]),
        "planning_only": False,
        "review_exempt_since_quiet_review": False,
        "_review_thread_nodes": list(threads["nodes"]),
        "_review_comments": list(node["comments"]["nodes"]),
        "_reviews": list(node["reviews"]["nodes"]),
        "_review_comment_page": node["comments"].get(
            "pageInfo", empty_page_info()
        ),
        "_review_page": node["reviews"].get(
            "pageInfo", empty_page_info()
        ),
        "_thread_page": threads["pageInfo"],
        "_check_page": contexts["pageInfo"],
        "_file_page": files["pageInfo"],
    }


def empty_page_info() -> dict[str, Any]:
    return {"hasNextPage": False, "endCursor": None}


def pagination_query(tasks: Sequence[PaginationTask]) -> tuple[str, dict[str, Any]]:
    declarations: list[str] = []
    selections: list[str] = []
    variables: dict[str, Any] = {}
    for index, task in enumerate(tasks):
        declarations.extend((f"$id{index}: ID!", f"$after{index}: String!"))
        variables[f"id{index}"] = task.pull_request["node_id"]
        variables[f"after{index}"] = task.cursor
        if task.kind == "threads":
            connection = (
                f"reviewThreads(first: 100, after: $after{index}) {{ "
                "nodes { id isResolved comments(first: 100) { totalCount "
                "nodes { author { login } authorAssociation body createdAt lastEditedAt pullRequestReview { id } } "
                "pageInfo { hasNextPage endCursor } } } "
                "pageInfo { hasNextPage endCursor } }"
            )
        elif task.kind == "checks":
            connection = (
                "commits(last: 1) { nodes { commit { statusCheckRollup { "
                f"contexts(first: 100, after: $after{index}) {{ nodes {{ __typename "
                "... on CheckRun { name status conclusion completedAt } "
                "... on StatusContext { context state createdAt } } "
                "pageInfo { hasNextPage endCursor } } } } } }"
            )
        else:
            connection = (
                f"files(first: 100, after: $after{index}) {{ "
                "nodes { path changeType additions deletions } pageInfo { hasNextPage endCursor } }"
            )
        selections.append(
            f"item{index}: node(id: $id{index}) {{ ... on PullRequest {{ {connection} }} }}"
        )
    return (
        f"query({', '.join(declarations)}) {{ {' '.join(selections)} }}",
        variables,
    )


def pagination_page(node: dict[str, Any], kind: str) -> dict[str, Any]:
    if not isinstance(node, dict):
        raise RuntimeError("pull request became unavailable during pagination")
    if kind == "threads":
        return node["reviewThreads"]
    if kind == "files":
        return node["files"]
    commit_nodes = node["commits"]["nodes"]
    rollup = commit_nodes[0]["commit"].get("statusCheckRollup") if commit_nodes else None
    return rollup["contexts"] if rollup else {"nodes": [], "pageInfo": empty_page_info()}


def check_name(check: dict[str, Any]) -> str:
    if check["__typename"] == "CheckRun":
        return check["name"]
    return check["context"]


def is_non_gating_check(check: dict[str, Any]) -> bool:
    name = check_name(check)
    return name.endswith("(report only)") or name.casefold() in NON_GATING_CHECK_NAMES


def check_is_green(check: dict[str, Any]) -> bool:
    if check["__typename"] == "CheckRun":
        return (
            check.get("status") == "COMPLETED"
            and check.get("conclusion") in GREEN_CHECK_RUN_CONCLUSIONS
        )
    return check.get("state") in GREEN_STATUS_CONTEXT_STATES


def check_observed_state(check: dict[str, Any]) -> str:
    if check["__typename"] == "CheckRun":
        return check.get("conclusion") or check.get("status") or "UNKNOWN"
    return check.get("state") or "UNKNOWN"


def evaluate_convergence(pull_request: dict[str, Any]) -> dict[str, Any]:
    unresolved_threads = sum(
        1
        for thread in pull_request["review_threads"]
        if not thread["isResolved"] and not thread.get("isEscalated", False)
    )
    escalated_threads = sum(
        1
        for thread in pull_request["review_threads"]
        if thread.get("isEscalated", False)
    )
    undispositioned_threads = sum(
        1
        for thread in pull_request["review_threads"]
        if not thread["isDispositioned"]
    )
    gating_checks = [check for check in pull_request["checks"] if not is_non_gating_check(check)]
    non_gating_checks = [check for check in pull_request["checks"] if is_non_gating_check(check)]
    reasons: list[str] = []
    if pull_request.get("is_draft", False):
        reasons.append("pull-request-is-draft")
    if unresolved_threads:
        reasons.append(f"unresolved-review-threads:{unresolved_threads}")
    if undispositioned_threads:
        reasons.append(
            f"undispositioned-review-threads:{undispositioned_threads}"
        )
    if (
        not pull_request.get("planning_only", False)
        and pull_request["head_oid"] not in pull_request["quiet_review_head_oids"]
        and not pull_request.get("review_exempt_since_quiet_review", False)
    ):
        reasons.append("quiet-review-not-completed-for-current-head")
    if "body" in pull_request:
        description = pull_request["body"]
        if len(re.findall(r"\b[\w'-]+\b", description)) > 350:
            reasons.append("description-exceeds-350-words")
    if pull_request["checked_head_oid"] != pull_request["head_oid"]:
        reasons.append("checks-not-for-current-head")
    if pull_request["check_rollup_state"] is None:
        reasons.append("check-rollup-missing")
    for check in gating_checks:
        if not check_is_green(check):
            reasons.append(f"check-not-green:{check_name(check)}:{check_observed_state(check)}")
    if pull_request["mergeable"] == "CONFLICTING":
        reasons.append("base-conflict")
    elif pull_request["mergeable"] != "MERGEABLE":
        reasons.append(f"mergeability-{str(pull_request['mergeable']).lower()}")
    if pull_request["base_commits_not_in_head"] is None:
        reasons.append("base-ancestry-unknown")
    elif pull_request["base_commits_not_in_head"]:
        reasons.append(
            "base-commits-not-in-head:"
            f"{pull_request['base_commits_not_in_head']}"
        )
    return {
        "converged": not reasons,
        "reasons": reasons,
        "unresolved_review_threads": unresolved_threads,
        "undispositioned_review_threads": undispositioned_threads,
        "escalated_review_threads": escalated_threads,
        "planning_only": pull_request.get("planning_only", False),
        "check_rollup_state": pull_request["check_rollup_state"],
        "base_commits_not_in_head": pull_request["base_commits_not_in_head"],
        "checks_green": (
            pull_request["check_rollup_state"] is not None
            and all(check_is_green(check) for check in gating_checks)
        ),
        "gating_checks": [
            {"name": check_name(check), "state": check_observed_state(check)}
            for check in gating_checks
        ],
        "non_gating_checks": [
            {"name": check_name(check), "state": check_observed_state(check)}
            for check in non_gating_checks
        ],
    }


def choose_decision(
    *,
    converged: bool,
    now: float,
    last_dispatched_at: float | None,
    cool_off_seconds: float,
    active_work: bool | None,
    dry_run: bool,
    dispatch_configured: bool,
) -> Decision:
    if converged:
        return Decision("merge-ready", "convergence-predicate-satisfied")
    if last_dispatched_at is not None:
        remaining = last_dispatched_at + cool_off_seconds - now
        if remaining > 0:
            return Decision("cooling-off", f"dispatch-cool-off:{remaining:.0f}s-remaining")
    if active_work is None:
        return Decision("active-check-required", "outside-dispatch-cool-off")
    if active_work:
        return Decision("already-active", "operator-command-reported-active-work")
    if dry_run:
        return Decision("would-dispatch", "dry-run-and-no-active-work")
    if not dispatch_configured:
        return Decision("skipped", "dispatch-command-not-configured")
    return Decision("dispatch", "no-active-work-and-outside-cool-off")


def default_state_file() -> Path:
    state_root = os.environ.get("XDG_STATE_HOME")
    base = Path(state_root) if state_root else Path.home() / ".local" / "state"
    return base / "signalbox" / "convergence-reconciler.json"


def load_config(argv: Sequence[str] | None = None) -> Config:
    bootstrap = argparse.ArgumentParser(add_help=False)
    bootstrap.add_argument("--config")
    bootstrap_args, _ = bootstrap.parse_known_args(argv)
    config_path = bootstrap_args.config or os.environ.get("CONVERGENCE_RECONCILER_CONFIG")
    file_values: dict[str, Any] = {}
    if config_path:
        with Path(config_path).open(encoding="utf-8") as stream:
            file_values = json.load(stream)
        if not isinstance(file_values, dict):
            raise ValueError("configuration file must contain a JSON object")

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", help="JSON configuration file")
    parser.add_argument("--repo", dest="repository")
    parser.add_argument("--head-pattern")
    parser.add_argument("--interval-seconds", type=float)
    parser.add_argument("--cool-off-seconds", type=float)
    parser.add_argument("--command-timeout-seconds", type=float)
    parser.add_argument("--state-file", type=Path)
    parser.add_argument("--log-file", type=Path)
    parser.add_argument("--active-command")
    parser.add_argument("--dispatch-command")
    parser.add_argument("--summary", choices=("text", "json", "none"))
    parser.add_argument("--dry-run", action="store_true", default=None)
    parser.add_argument("--once", action="store_true", default=None)
    args = parser.parse_args(argv)

    def selected(name: str, default: Any = None) -> Any:
        cli_value = getattr(args, name)
        if cli_value is not None:
            return cli_value
        env_name = f"CONVERGENCE_RECONCILER_{name.upper()}"
        if env_name in os.environ:
            return os.environ[env_name]
        return file_values.get(name, default)

    repository = selected("repository")
    if not repository:
        parser.error("repository is required via --repo, environment, or config")
    split_repository(str(repository))
    dry_run = parse_bool(selected("dry_run", False), "dry_run")
    once = parse_bool(selected("once", False), "once")
    active_command = parse_command(selected("active_command"), "active_command")
    dispatch_command = parse_command(selected("dispatch_command"), "dispatch_command")
    if not active_command:
        parser.error("active_command is required")
    if not dry_run and not dispatch_command:
        parser.error("dispatch_command is required unless dry-run is enabled")
    interval_seconds = positive_number(selected("interval_seconds", 300), "interval_seconds")
    cool_off_seconds = nonnegative_number(selected("cool_off_seconds", 1800), "cool_off_seconds")
    command_timeout_seconds = positive_number(
        selected("command_timeout_seconds", 60), "command_timeout_seconds"
    )
    state_file = configured_path(
        selected("state_file", default_state_file()), "state_file"
    )
    log_file_value = selected("log_file")
    log_file = (
        configured_path(log_file_value, "log_file")
        if log_file_value is not None
        else None
    )
    return Config(
        repository=str(repository),
        head_pattern=str(selected("head_pattern", "agent/*")),
        interval_seconds=interval_seconds,
        cool_off_seconds=cool_off_seconds,
        command_timeout_seconds=command_timeout_seconds,
        state_file=state_file,
        log_file=log_file,
        active_command=active_command,
        dispatch_command=dispatch_command,
        summary=str(selected("summary", "text")),
        dry_run=dry_run,
        once=once,
    )


def configured_path(value: Any, name: str) -> Path:
    if isinstance(value, (str, os.PathLike)):
        return Path(value)
    raise ValueError(f"{name} must be a filesystem path")


def parse_bool(value: Any, name: str) -> bool:
    if isinstance(value, bool):
        return value
    if isinstance(value, str) and value.casefold() in {"1", "true", "yes", "on"}:
        return True
    if isinstance(value, str) and value.casefold() in {"0", "false", "no", "off"}:
        return False
    raise ValueError(f"{name} must be a boolean")


def parse_command(value: Any, name: str) -> tuple[str, ...]:
    if value is None:
        return ()
    if isinstance(value, str):
        return tuple(shlex.split(value))
    if isinstance(value, list) and all(isinstance(part, str) for part in value):
        return tuple(value)
    raise ValueError(f"{name} must be a shell-like string or an array of strings")


def positive_number(value: Any, name: str) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError) as error:
        raise ValueError(f"{name} must be a finite number greater than zero") from error
    if not math.isfinite(number) or number <= 0:
        raise ValueError(f"{name} must be a finite number greater than zero")
    return number


def nonnegative_number(value: Any, name: str) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError) as error:
        raise ValueError(f"{name} must be a finite nonnegative number") from error
    if not math.isfinite(number) or number < 0:
        raise ValueError(f"{name} must be a finite nonnegative number")
    return number


def load_state(path: Path, repository: str) -> dict[str, Any]:
    if not path.exists():
        return {
            "version": STATE_VERSION,
            "repository": repository,
            "pull_requests": {},
        }
    with path.open(encoding="utf-8") as stream:
        state = json.load(stream)
    valid_shape = (
        isinstance(state, dict)
        and state.get("version") == STATE_VERSION
        and isinstance(state.get("repository"), str)
        and isinstance(state.get("pull_requests"), dict)
    )
    if not valid_shape:
        raise ValueError(f"unsupported or malformed state file: {path}")
    if not same_repository(state.get("repository"), repository):
        raise ValueError(f"state file belongs to another repository: {path}")
    for number, record in state["pull_requests"].items():
        if not isinstance(number, str) or not number.isdecimal() or int(number) < 1:
            raise ValueError(f"unsupported or malformed state file: {path}")
        if not isinstance(record, dict):
            raise ValueError(f"unsupported or malformed state file: {path}")
        for field in (
            "node_id",
            "head_ref",
            "head_oid",
            "terminal_state",
            "terminal_at",
            "authenticated_review_head",
            "authenticated_review_id",
            "authenticated_review_body",
            "last_dispatched_head",
        ):
            value = record.get(field)
            if value is not None and not isinstance(value, str):
                raise ValueError(f"unsupported or malformed state file: {path}")
        resolution_times = record.get("resolved_thread_observed_at")
        if resolution_times is not None and (
            not isinstance(resolution_times, dict)
            or not all(
                isinstance(thread_id, str) and isinstance(observed_at, str)
                for thread_id, observed_at in resolution_times.items()
            )
        ):
            raise ValueError(f"unsupported or malformed state file: {path}")
        for field in ("known_codex_review_ids", "review_wave_ids"):
            value = record.get(field)
            if value is not None and (
                not isinstance(value, list)
                or not all(isinstance(item, str) for item in value)
            ):
                raise ValueError(f"unsupported or malformed state file: {path}")
        review_wave_base_oid = record.get("review_wave_base_oid")
        if review_wave_base_oid is not None and not isinstance(
            review_wave_base_oid, str
        ):
            raise ValueError(f"unsupported or malformed state file: {path}")
        for field in ("unconverged_since", "idle_since", "last_dispatched_at"):
            value = record.get(field)
            if (
                value is not None
                and (
                    isinstance(value, bool)
                    or not isinstance(value, (int, float))
                    or not math.isfinite(value)
                    or value < 0
                )
            ):
                raise ValueError(f"unsupported or malformed state file: {path}")
    return state


def save_state(path: Path, state: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
            json.dump(state, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary_name, path)
        directory_descriptor = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory_descriptor)
        finally:
            os.close(directory_descriptor)
    except BaseException:
        try:
            os.unlink(temporary_name)
        except FileNotFoundError:
            pass
        raise


def utc_timestamp(epoch_seconds: float) -> str:
    timestamp = dt.datetime.fromtimestamp(epoch_seconds, tz=dt.timezone.utc)
    return timestamp.isoformat().replace("+00:00", "Z")


def duration_since(start: float | None, now: float) -> int | None:
    return max(0, round(now - start)) if start is not None else None


def run_operator_command(
    command: Sequence[str], pull_request_number: int, computed_state: dict[str, Any], timeout: float
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            *command,
            str(pull_request_number),
            json.dumps(computed_state, sort_keys=True, separators=(",", ":")),
        ],
        text=True,
        capture_output=True,
        check=False,
        timeout=timeout,
    )


def command_detail(completed: subprocess.CompletedProcess[str]) -> str | None:
    output = completed.stdout.strip() or completed.stderr.strip()
    return output[:512] if output else None


def decision_record(
    config: Config,
    pull_request: dict[str, Any],
    computed: dict[str, Any],
    record: dict[str, Any],
    decision: Decision,
    now: float,
) -> dict[str, Any]:
    return {
        "event": "decision",
        "timestamp": utc_timestamp(now),
        "repository": config.repository,
        "pull_request": pull_request["number"],
        "head_ref": pull_request["head_ref"],
        "head_oid": pull_request["head_oid"],
        "decision": decision.name,
        "reason": decision.reason,
        "convergence_reasons": computed["reasons"],
        "unconverged_since": (
            utc_timestamp(record["unconverged_since"])
            if record.get("unconverged_since") is not None
            else None
        ),
        "unconverged_for_seconds": duration_since(record.get("unconverged_since"), now),
        "idle_since": (
            utc_timestamp(record["idle_since"]) if record.get("idle_since") is not None else None
        ),
        "idle_for_seconds": duration_since(record.get("idle_since"), now),
    }


def computed_command_state(
    pull_request: dict[str, Any], computed: dict[str, Any], record: dict[str, Any], now: float
) -> dict[str, Any]:
    return {
        "number": pull_request["number"],
        "title": pull_request["title"],
        "url": pull_request["url"],
        "is_draft": pull_request["is_draft"],
        "base_ref": pull_request["base_ref"],
        "head_ref": pull_request["head_ref"],
        "head_oid": pull_request["head_oid"],
        "checked_head_oid": pull_request["checked_head_oid"],
        "mergeable": pull_request["mergeable"],
        **computed,
        "unconverged_since": record.get("unconverged_since"),
        "unconverged_for_seconds": duration_since(record.get("unconverged_since"), now),
        "idle_since": record.get("idle_since"),
        "idle_for_seconds": duration_since(record.get("idle_since"), now),
        "last_dispatched_at": record.get("last_dispatched_at"),
    }


def process_pull_request(
    config: Config,
    logger: JsonLogger,
    state: dict[str, Any],
    pull_request: dict[str, Any],
    now: float,
) -> dict[str, Any]:
    records = state["pull_requests"]
    record = records.setdefault(str(pull_request["number"]), {})
    record.update(
        {
            "node_id": pull_request["node_id"],
            "head_ref": pull_request["head_ref"],
            "head_oid": pull_request["head_oid"],
            "terminal_state": None,
        }
    )
    if pull_request["head_oid"] in pull_request["quiet_review_head_oids"]:
        record["authenticated_review_head"] = pull_request["head_oid"]
        record["authenticated_review_body"] = pull_request.get("body", "")
        review_id = pull_request.get("authenticated_review_ids", {}).get(
            pull_request["head_oid"]
        )
        if isinstance(review_id, str):
            record["authenticated_review_id"] = review_id
    resolution_times = record.setdefault("resolved_thread_observed_at", {})
    observed_at = utc_timestamp(now)
    current_thread_ids = {
        thread["id"]
        for thread in pull_request.get("review_threads", [])
        if isinstance(thread.get("id"), str)
    }
    for thread_id in list(resolution_times):
        if thread_id not in current_thread_ids:
            del resolution_times[thread_id]
    for thread in pull_request.get("review_threads", []):
        thread_id = thread.get("id")
        if not isinstance(thread_id, str):
            continue
        if thread.get("isResolved"):
            resolution_times.setdefault(thread_id, observed_at)
        else:
            resolution_times.pop(thread_id, None)
    record["known_codex_review_ids"] = pull_request.get(
        "known_codex_review_ids", []
    )
    record["review_wave_ids"] = pull_request.get("review_wave_ids", [])
    base_oid = pull_request.get("base_oid")
    if isinstance(base_oid, str):
        record["review_wave_base_oid"] = base_oid
    computed = evaluate_convergence(pull_request)
    if computed["converged"]:
        record["last_dispatched_at"] = None
        record["last_dispatched_head"] = None
    applicable_dispatch = (
        record.get("last_dispatched_at")
        if record.get("last_dispatched_head") == pull_request["head_oid"]
        else None
    )
    if not computed["converged"] and record.get("unconverged_since") is None:
        record["unconverged_since"] = now
    clear_unconverged_after_log = computed["converged"]
    clear_idle_after_log = computed["converged"]

    decision = choose_decision(
        converged=computed["converged"],
        now=now,
        last_dispatched_at=applicable_dispatch,
        cool_off_seconds=config.cool_off_seconds,
        active_work=None,
        dry_run=config.dry_run,
        dispatch_configured=bool(config.dispatch_command),
    )
    command_state = computed_command_state(pull_request, computed, record, now)
    detail: str | None = None
    if decision.name == "active-check-required":
        try:
            active_result = run_operator_command(
                config.active_command,
                pull_request["number"],
                command_state,
                config.command_timeout_seconds,
            )
        except subprocess.TimeoutExpired:
            decision = Decision("skipped", "active-command-timed-out")
        except OSError as error:
            decision = Decision("skipped", f"active-command-start-failed:{error.errno}")
            detail = str(error)[:512]
        else:
            if active_result.returncode not in (0, 1):
                decision = Decision("skipped", f"active-command-exited:{active_result.returncode}")
                detail = command_detail(active_result)
            else:
                active_work = active_result.returncode == 0
                if active_work:
                    clear_idle_after_log = True
                elif record.get("idle_since") is None:
                    record["idle_since"] = now
                command_state = computed_command_state(pull_request, computed, record, now)
                decision = choose_decision(
                    converged=False,
                    now=now,
                    last_dispatched_at=applicable_dispatch,
                    cool_off_seconds=config.cool_off_seconds,
                    active_work=active_work,
                    dry_run=config.dry_run,
                    dispatch_configured=bool(config.dispatch_command),
                )
    if decision.name == "dispatch":
        previous_dispatch = record.get("last_dispatched_at")
        previous_dispatch_head = record.get("last_dispatched_head")
        now = time.time()
        record["last_dispatched_at"] = now
        record["last_dispatched_head"] = pull_request["head_oid"]
        command_state = computed_command_state(pull_request, computed, record, now)
        save_state(config.state_file, state)
        try:
            dispatch_result = run_operator_command(
                config.dispatch_command,
                pull_request["number"],
                command_state,
                config.command_timeout_seconds,
            )
        except subprocess.TimeoutExpired:
            decision = Decision(
                "skipped", "dispatch-command-timed-out-cool-off-retained"
            )
        except OSError as error:
            record["last_dispatched_at"] = previous_dispatch
            record["last_dispatched_head"] = previous_dispatch_head
            save_state(config.state_file, state)
            decision = Decision(
                "skipped", f"dispatch-command-start-failed:{error.errno}"
            )
            detail = str(error)[:512]
        else:
            detail = command_detail(dispatch_result)
            if dispatch_result.returncode == 0:
                decision = Decision("dispatched", "operator-command-accepted-dispatch")
                clear_idle_after_log = True
            else:
                decision = Decision(
                    "skipped",
                    f"dispatch-command-exited:{dispatch_result.returncode}-cool-off-retained",
                )
    log_record = decision_record(config, pull_request, computed, record, decision, now)
    if detail:
        log_record["operator_output"] = detail
    logger.write(log_record)
    if clear_unconverged_after_log:
        record["unconverged_since"] = None
    if clear_idle_after_log:
        record["idle_since"] = None
    return {
        "pull_request": pull_request["number"],
        "decision": decision.name,
        "reason": decision.reason,
        "idle_for_seconds": log_record["idle_for_seconds"],
    }


def record_terminal_pull_requests(
    config: Config,
    logger: JsonLogger,
    state: dict[str, Any],
    tracked: Sequence[dict[str, Any]],
    now: float,
) -> list[dict[str, Any]]:
    summaries: list[dict[str, Any]] = []
    records = state["pull_requests"]
    for pull_request in tracked:
        if pull_request["state"] == "OPEN":
            continue
        record = records.get(str(pull_request["number"]), {})
        if record.get("terminal_state") == pull_request["state"]:
            continue
        unconverged_since = record.get("unconverged_since")
        idle_since = record.get("idle_since")
        record.update(
            {
                "node_id": pull_request["id"],
                "head_ref": pull_request.get("headRefName"),
                "head_oid": pull_request.get("headRefOid"),
                "terminal_state": pull_request["state"],
                "terminal_at": pull_request.get("mergedAt") or pull_request.get("closedAt"),
            }
        )
        records[str(pull_request["number"])] = record
        reason = (
            "pull-request-merged"
            if pull_request["state"] == "MERGED"
            else "pull-request-closed"
        )
        logger.write(
            {
                "event": "decision",
                "timestamp": utc_timestamp(now),
                "repository": config.repository,
                "pull_request": pull_request["number"],
                "head_ref": pull_request.get("headRefName"),
                "head_oid": pull_request.get("headRefOid"),
                "decision": "skipped",
                "reason": reason,
                "convergence_reasons": [],
                "unconverged_since": (
                    utc_timestamp(unconverged_since)
                    if unconverged_since is not None
                    else None
                ),
                "unconverged_for_seconds": duration_since(unconverged_since, now),
                "idle_since": utc_timestamp(idle_since) if idle_since is not None else None,
                "idle_for_seconds": duration_since(idle_since, now),
            }
        )
        record["unconverged_since"] = None
        record["idle_since"] = None
        summaries.append(
            {
                "pull_request": pull_request["number"],
                "decision": "skipped",
                "reason": reason,
                "idle_for_seconds": duration_since(idle_since, now),
            }
        )
    return summaries


def run_tick(config: Config, logger: JsonLogger) -> list[dict[str, Any]]:
    state = load_state(config.state_file, config.repository)
    active_records = [
        record
        for record in state["pull_requests"].values()
        if record.get("node_id") and not record.get("terminal_state")
    ]
    tracked_node_ids = [record["node_id"] for record in active_records]
    persisted_records = {
        int(number): record
        for number, record in state["pull_requests"].items()
    }
    client = GitHubGraphQL(config.repository, config.command_timeout_seconds)
    pull_requests, tracked = client.snapshot(
        tracked_node_ids, config.head_pattern, persisted_records
    )
    now = time.time()
    summaries = record_terminal_pull_requests(config, logger, state, tracked, now)
    for pull_request in pull_requests:
        client._verify_snapshot_oids([pull_request], raise_on_change=True)
        authorized = same_repository(pull_request["head_repository"], config.repository)
        watched = authorized and fnmatch.fnmatchcase(
            pull_request["head_ref"], config.head_pattern
        )
        if watched:
            summaries.append(
                process_pull_request(
                    config, logger, state, pull_request, time.time()
                )
            )
            continue
        existing = state["pull_requests"].get(str(pull_request["number"]))
        if existing and not existing.get("terminal_state"):
            unconverged_since = existing.get("unconverged_since")
            idle_since = existing.get("idle_since")
            existing["terminal_state"] = "UNWATCHED"
            logger.write(
                {
                    "event": "decision",
                    "timestamp": utc_timestamp(now),
                    "repository": config.repository,
                    "pull_request": pull_request["number"],
                    "head_ref": pull_request["head_ref"],
                    "head_oid": pull_request["head_oid"],
                    "decision": "skipped",
                    "reason": (
                        "head-source-repository-not-authorized"
                        if not authorized
                        else "head-branch-no-longer-matches-pattern"
                    ),
                    "convergence_reasons": [],
                    "unconverged_since": (
                        utc_timestamp(unconverged_since)
                        if unconverged_since is not None
                        else None
                    ),
                    "unconverged_for_seconds": duration_since(unconverged_since, now),
                    "idle_since": (
                        utc_timestamp(idle_since) if idle_since is not None else None
                    ),
                    "idle_for_seconds": duration_since(idle_since, now),
                }
            )
            existing["unconverged_since"] = None
            existing["idle_since"] = None
            summaries.append(
                {
                    "pull_request": pull_request["number"],
                    "decision": "skipped",
                    "reason": (
                        "head-source-repository-not-authorized"
                        if not authorized
                        else "head-branch-no-longer-matches-pattern"
                    ),
                    "idle_for_seconds": duration_since(idle_since, now),
                }
            )
    save_state(config.state_file, state)
    return summaries


def print_summary(summaries: Sequence[dict[str, Any]], mode: str) -> None:
    if mode == "none":
        return
    if mode == "json":
        print(json.dumps(list(summaries), indent=2, sort_keys=True))
        return
    if not summaries:
        print("No watched pull requests.")
        return
    print("PR     decision         idle(s)  reason")
    for summary in summaries:
        idle = "-" if summary["idle_for_seconds"] is None else str(summary["idle_for_seconds"])
        print(
            f"#{summary['pull_request']:<5} {summary['decision']:<16} "
            f"{idle:<8} {summary['reason']}"
        )


def main(argv: Sequence[str] | None = None) -> int:
    try:
        config = load_config(argv)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"configuration error: {error}", file=sys.stderr)
        return 2
    try:
        logger = JsonLogger(config.log_file)
    except OSError as error:
        print(f"configuration error: {error}", file=sys.stderr)
        return 2
    try:
        while True:
            try:
                summaries = run_tick(config, logger)
            except (OSError, RuntimeError, ValueError, json.JSONDecodeError) as error:
                logger.write(
                    {
                        "event": "tick-error",
                        "timestamp": utc_timestamp(time.time()),
                        "repository": config.repository,
                        "reason": str(error),
                    }
                )
                if config.once:
                    return 1
            else:
                print_summary(summaries, config.summary)
            if config.once:
                return 0
            time.sleep(config.interval_seconds)
    except KeyboardInterrupt:
        return 130
    finally:
        logger.close()


if __name__ == "__main__":
    raise SystemExit(main())

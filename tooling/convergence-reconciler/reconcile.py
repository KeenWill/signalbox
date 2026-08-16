#!/usr/bin/env python3
"""Keep watched GitHub pull requests moving toward convergence."""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import fnmatch
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
          isResolved
          comments(first: 100) {
            totalCount
            nodes { author { login } body }
          }
        }
        pageInfo { hasNextPage endCursor }
      }
      comments(last: 100) {
        nodes { author { login } authorAssociation body createdAt }
      }
      reviews(last: 100) {
        nodes {
          author { login }
          submittedAt
          commit { oid }
          comments(first: 1) { totalCount }
        }
      }
      files(first: 100) {
        nodes { path changeType }
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
                  ... on CheckRun { name status conclusion }
                  ... on StatusContext { context state }
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
    r"^fixed in commits?\s+`?[0-9a-f]{7,40}`?", re.IGNORECASE
)
ESCALATION_DISPOSITION = "escalated without disposition"
NON_GATING_CHECK_NAMES = frozenset(
    {
        "coderabbit",
        "codecov/patch",
        "codecov/project",
        "comment the coverage report",
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

    def snapshot(
        self, tracked_node_ids: Sequence[str], head_pattern: str
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
            connection = data["repository"]["pullRequests"]
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
                pull_requests.append(normalize_pull_request(node))
        self._finish_paginated_connections(pull_requests)
        self._load_base_ancestry(pull_requests)
        self._load_planning_only_status(pull_requests)
        return pull_requests, tracked

    def _load_base_ancestry(
        self, pull_requests: list[dict[str, Any]]
    ) -> None:
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
            repository = data["repository"]
            verification_declarations: list[str] = []
            verification_selections: list[str] = []
            verification_variables: dict[str, Any] = {}
            for index, pull_request in enumerate(batch):
                variable = f"node{index}"
                verification_declarations.append(f"${variable}: ID!")
                verification_variables[variable] = pull_request["node_id"]
                verification_selections.append(
                    f"state{index}: node(id: ${variable}) {{ ... on PullRequest "
                    "{ baseRefOid headRefOid } }"
                )
            verification_query = (
                f"query({', '.join(verification_declarations)}) {{ "
                f"{' '.join(verification_selections)} }}"
            )
            verification = self.execute(
                verification_query, verification_variables
            )
            for index, pull_request in enumerate(batch):
                comparison = repository[f"item{index}"]
                current = verification[f"state{index}"]
                snapshot_still_current = (
                    current is not None
                    and current["baseRefOid"] == pull_request["base_oid"]
                    and current["headRefOid"] == pull_request["head_oid"]
                )
                pull_request["base_commits_not_in_head"] = (
                    comparison["behindBy"]
                    if comparison is not None and snapshot_still_current
                    else None
                )

    def _load_planning_only_status(
        self, pull_requests: list[dict[str, Any]]
    ) -> None:
        for pull_request in pull_requests:
            changed_files = pull_request["changed_files"]
            if not changed_files:
                pull_request["planning_only"] = False
                continue
            declarations = ["$owner: String!", "$name: String!"]
            selections: list[str] = []
            variables: dict[str, Any] = {
                "owner": self.owner,
                "name": self.name,
            }
            for index, changed_file in enumerate(changed_files):
                head_variable = f"head{index}"
                base_variable = f"base{index}"
                declarations.extend(
                    (f"${head_variable}: String!", f"${base_variable}: String!")
                )
                variables[head_variable] = (
                    f"{pull_request['head_oid']}:{changed_file['path']}"
                )
                variables[base_variable] = (
                    f"{pull_request['base_oid']}:{changed_file['path']}"
                )
                selections.extend(
                    (
                        f"head{index}: object(expression: ${head_variable}) "
                        "{ ... on Blob { text } }",
                        f"base{index}: object(expression: ${base_variable}) "
                        "{ ... on Blob { text } }",
                    )
                )
            query = (
                f"query({', '.join(declarations)}) {{ repository("
                f"owner: $owner, name: $name) {{ {' '.join(selections)} }} }}"
            )
            repository = self.execute(query, variables)["repository"]
            planning_only = True
            for index, changed_file in enumerate(changed_files):
                head_has_banner = blob_has_planning_banner(
                    repository[f"head{index}"]
                )
                base_has_banner = blob_has_planning_banner(
                    repository[f"base{index}"]
                )
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
                page = pagination_page(data[f"item{index}"], task.kind)
                if task.kind == "threads":
                    task.pull_request["review_threads"].extend(
                        normalize_review_threads(
                            page["nodes"],
                            task.pull_request["author_login"],
                        )
                    )
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


def disposition_kind(body: str) -> str | None:
    stripped = body.strip()
    if FIX_DISPOSITION.match(stripped):
        return "fixed"
    if stripped.casefold().startswith("declined:") and stripped[9:].strip():
        return "declined"
    if stripped.casefold() == ESCALATION_DISPOSITION:
        return "escalated"
    return None


def is_codex_review_request(comment: dict[str, Any], head_oid: str) -> bool:
    body = comment.get("body") or ""
    requests_review = any(
        line.strip().casefold().startswith("@codex review")
        for line in body.splitlines()
    )
    return (
        comment.get("authorAssociation")
        in TRUSTED_REVIEW_REQUEST_ASSOCIATIONS
        and requests_review
        and head_oid.casefold() in body.casefold()
    )


def normalize_review_threads(
    threads: Sequence[dict[str, Any]], pull_request_author: str | None
) -> list[dict[str, Any]]:
    normalized: list[dict[str, Any]] = []
    for thread in threads:
        comments = thread["comments"]["nodes"]
        dispositions = [
            disposition_kind(comment.get("body") or "")
            for comment in comments[1:]
            if pull_request_author is not None
            and author_login(comment) is not None
            and author_login(comment).casefold() == pull_request_author.casefold()
        ]
        dispositioned = any(kind is not None for kind in dispositions)
        normalized.append(
            {
                "isResolved": thread["isResolved"],
                "isDispositioned": dispositioned,
                "isEscalated": "escalated" in dispositions,
            }
        )
    return normalized


def normalize_pull_request(node: dict[str, Any]) -> dict[str, Any]:
    commit_nodes = node["commits"]["nodes"]
    commit = commit_nodes[0]["commit"] if commit_nodes else None
    rollup = commit.get("statusCheckRollup") if commit else None
    contexts = rollup["contexts"] if rollup else {"nodes": [], "pageInfo": empty_page_info()}
    threads = node["reviewThreads"]
    files = node["files"]
    pull_request_author = author_login(node)
    review_request_times = [
        comment["createdAt"]
        for comment in node["comments"]["nodes"]
        if is_codex_review_request(comment, node["headRefOid"])
    ]
    latest_review_request = max(review_request_times, default=None)
    return {
        "node_id": node["id"],
        "number": node["number"],
        "state": node["state"],
        "title": node["title"],
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
        "review_threads": normalize_review_threads(
            threads["nodes"], pull_request_author
        ),
        "quiet_review_head_oids": [
            review["commit"]["oid"]
            for review in node["reviews"]["nodes"]
            if review.get("commit")
            and author_login(review) is not None
            and author_login(review).casefold()
            == CODEX_REVIEWER_LOGIN.casefold()
            and latest_review_request is not None
            and review.get("submittedAt") is not None
            and review["submittedAt"] >= latest_review_request
            and review["comments"]["totalCount"] == 0
        ],
        "check_rollup_state": rollup.get("state") if rollup else None,
        "checks": list(contexts["nodes"]),
        "changed_files": list(files["nodes"]),
        "planning_only": False,
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
                "nodes { isResolved comments(first: 100) { totalCount "
                "nodes { author { login } body } } } "
                "pageInfo { hasNextPage endCursor } }"
            )
        elif task.kind == "checks":
            connection = (
                "commits(last: 1) { nodes { commit { statusCheckRollup { "
                f"contexts(first: 100, after: $after{index}) {{ nodes {{ __typename "
                "... on CheckRun { name status conclusion } "
                "... on StatusContext { context state } } "
                "pageInfo { hasNextPage endCursor } } } } } }"
            )
        else:
            connection = (
                f"files(first: 100, after: $after{index}) {{ "
                "nodes { path changeType } pageInfo { hasNextPage endCursor } }"
            )
        selections.append(
            f"item{index}: node(id: $id{index}) {{ ... on PullRequest {{ {connection} }} }}"
        )
    return (
        f"query({', '.join(declarations)}) {{ {' '.join(selections)} }}",
        variables,
    )


def pagination_page(node: dict[str, Any], kind: str) -> dict[str, Any]:
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
    if unresolved_threads:
        reasons.append(f"unresolved-review-threads:{unresolved_threads}")
    if undispositioned_threads:
        reasons.append(
            f"undispositioned-review-threads:{undispositioned_threads}"
        )
    if (
        not pull_request.get("planning_only", False)
        and pull_request["head_oid"] not in pull_request["quiet_review_head_oids"]
    ):
        reasons.append("quiet-review-not-completed-for-current-head")
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
    return Config(
        repository=str(repository),
        head_pattern=str(selected("head_pattern", "agent/*")),
        interval_seconds=interval_seconds,
        cool_off_seconds=cool_off_seconds,
        command_timeout_seconds=command_timeout_seconds,
        state_file=Path(selected("state_file", default_state_file())),
        log_file=Path(selected("log_file")) if selected("log_file") else None,
        active_command=active_command,
        dispatch_command=dispatch_command,
        summary=str(selected("summary", "text")),
        dry_run=dry_run,
        once=once,
    )


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
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise ValueError(f"{name} must be a finite number greater than zero")
    return number


def nonnegative_number(value: Any, name: str) -> float:
    number = float(value)
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
        and isinstance(state.get("pull_requests"), dict)
    )
    if not valid_shape:
        raise ValueError(f"unsupported or malformed state file: {path}")
    if not same_repository(state.get("repository"), repository):
        raise ValueError(f"state file belongs to another repository: {path}")
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
    computed = evaluate_convergence(pull_request)
    if not computed["converged"] and record.get("unconverged_since") is None:
        record["unconverged_since"] = now
    clear_unconverged_after_log = computed["converged"]
    clear_idle_after_log = computed["converged"]

    decision = choose_decision(
        converged=computed["converged"],
        now=now,
        last_dispatched_at=record.get("last_dispatched_at"),
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
                    last_dispatched_at=record.get("last_dispatched_at"),
                    cool_off_seconds=config.cool_off_seconds,
                    active_work=active_work,
                    dry_run=config.dry_run,
                    dispatch_configured=bool(config.dispatch_command),
                )
    if decision.name == "dispatch":
        previous_dispatch = record.get("last_dispatched_at")
        now = time.time()
        record["last_dispatched_at"] = now
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
    pull_requests, tracked = GitHubGraphQL(
        config.repository, config.command_timeout_seconds
    ).snapshot(tracked_node_ids, config.head_pattern)
    now = time.time()
    summaries = record_terminal_pull_requests(config, logger, state, tracked, now)
    for pull_request in pull_requests:
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

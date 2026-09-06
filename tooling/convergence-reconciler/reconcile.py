#!/usr/bin/env python3
"""Keep watched GitHub pull requests moving using the shared convergence CLI."""
from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import fnmatch
import json
import math
import os
from pathlib import Path
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



TERMINAL_PULL_REQUESTS_QUERY = """
query($tracked: [ID!]!) {
  tracked: nodes(ids: $tracked) {
    ... on PullRequest { id number state mergedAt closedAt headRefName headRefOid }
  }
}
"""



STATE_VERSION = 1



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

    convergence_policy: Path = Path(__file__).resolve().parents[2] / "crates/convergence/examples/repository.toml"



@dataclasses.dataclass(frozen=True)
class Decision:
    name: str
    reason: str



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
        self,
        tracked_node_ids: Sequence[str],
        head_pattern: str,
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
        candidates = [
            node for node in open_pull_requests
            if node["id"] in tracked_ids or (
                same_repository(head_repository(node), self.repository)
                and fnmatch.fnmatchcase(node.get("headRefName") or "", head_pattern)
            )
        ]
        return candidates, tracked



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



def project_evaluation(evaluation: dict[str, Any]) -> dict[str, Any]:
    identity = evaluation["pull_request"]
    return {
        **evaluation["facts"],
        "node_id": identity["id"],
        "number": identity["number"],
        "title": identity["title"],
        "url": identity["url"],
        "base_ref": identity["baseRefName"],
        "head_ref": identity["headRefName"],
        "head_repository": head_repository(identity),
        "_evaluation": evaluation,
    }


def evaluate_current(config: Config, number: int, previous: dict[str, Any]) -> dict[str, Any]:
    with tempfile.TemporaryDirectory(prefix="convergence-evaluation-") as directory:
        state = Path(directory) / "state.json"
        state.write_text(json.dumps(previous), encoding="utf-8")
        try:
            completed = subprocess.run(
                ["signalbox-converge", "evaluate", "--pr", str(number),
                 "--repo", config.repository, "--policy", str(config.convergence_policy),
                 "--state", str(state)],
                text=True, capture_output=True, check=False,
                timeout=config.command_timeout_seconds,
            )
        except subprocess.TimeoutExpired as error:
            raise RuntimeError("convergence evaluation timed out") from error
        if completed.returncode not in (0, 1):
            raise RuntimeError(f"convergence evaluation failed: {command_detail(completed)}")
        try:
            evaluation = json.loads(completed.stdout)
        except json.JSONDecodeError as error:
            raise RuntimeError("convergence evaluation returned invalid JSON") from error
    return project_evaluation(evaluation)



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
    parser.add_argument("--policy", dest="convergence_policy", type=Path)
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
        convergence_policy=configured_path(selected("convergence_policy", Config.convergence_policy), "convergence_policy"),
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
    evaluation = pull_request["_evaluation"]
    record.update(evaluation["state"])
    record.update(
        {
            "node_id": pull_request["node_id"],
            "head_ref": pull_request["head_ref"],
            "head_oid": pull_request["head_oid"],
            "terminal_state": None,
        }
    )
    computed = {key: value for key, value in evaluation.items() if key not in ("facts", "state", "pull_request")}
    computed.update({key: pull_request[key] for key in ("planning_only", "check_rollup_state", "base_commits_not_in_head")})
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
    client = GitHubGraphQL(config.repository, config.command_timeout_seconds)
    pull_requests, tracked = client.snapshot(
        tracked_node_ids, config.head_pattern
    )
    now = time.time()
    summaries = record_terminal_pull_requests(config, logger, state, tracked, now)
    for pull_request in pull_requests:
        number = pull_request["number"]
        pull_request = evaluate_current(config, number, state["pull_requests"].get(str(number), {}))
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

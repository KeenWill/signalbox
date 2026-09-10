#!/usr/bin/env python3
"""Report observed test-result reuse and retries from Bazel's JSON event stream."""

from __future__ import annotations

import argparse
from collections.abc import Iterable, Iterator
from dataclasses import dataclass
import json
from pathlib import Path
from typing import Any


@dataclass
class TestAttempts:
    local: int = 0
    remote: int = 0
    executed: int = 0
    retries: int = 0
    seconds: float = 0.0
    duration_known: bool = True
    status: str = "No final summary"


def read_events(lines: Iterable[str]) -> Iterator[dict[str, Any] | None]:
    """Read complete records; None marks a truncated trailing JSON record."""
    records = (line for line in lines if line.strip())
    for line in records:
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            # Interior corruption must not silently discard later observations.
            if next(records, None) is not None:
                raise
            yield None
            return
        yield event


def summarize(events: Iterable[dict[str, Any] | None]) -> str:
    """Count observed attempts; cached historical durations are never execution."""
    targets: dict[str, TestAttempts] = {}
    complete = False
    for event in events:
        if event is None:
            complete = False
            continue
        complete = event.get("lastMessage", False)
        result = event.get("testResult")
        identity = event.get("id", {}).get("testResult")
        if result is not None and identity is not None:
            attempts = targets.setdefault(identity["label"], TestAttempts())
            if result.get("cachedLocally", False):
                attempts.local += 1
            elif result.get("executionInfo", {}).get("cachedRemotely", False):
                attempts.remote += 1
            else:
                attempts.executed += 1
                attempts.retries += int(identity.get("attempt", 1) > 1)
                duration = result.get("testAttemptDuration")
                milliseconds = result.get("testAttemptDurationMillis")
                if duration is not None:
                    attempts.seconds += float(duration.removesuffix("s"))
                elif milliseconds is not None:
                    attempts.seconds += float(milliseconds) / 1000
                else:
                    attempts.duration_known = False
        summary = event.get("testSummary")
        identity = event.get("id", {}).get("testSummary")
        if summary is not None and identity is not None:
            attempts = targets.setdefault(identity["label"], TestAttempts())
            attempts.status = summary.get("overallStatus", "Unknown")
    lines = ["## PostgreSQL test-result cache", ""]
    if not complete:
        lines.extend(["Partial event stream: the final BEP message was not received intact.", ""])
    if not targets:
        lines.append("No test-result events were observed; cache reuse is unmeasured.")
        return "\n".join(lines) + "\n"
    lines.extend([
        "Observed attempts below include retries. Cached durations from earlier runs are excluded.",
        "Execution seconds sum test-action durations; they are not job wall time.", "",
        "| Target | Local hits | Remote hits | Executed | Retries | Execution seconds | Final status |",
        "|---|---:|---:|---:|---:|---:|---|",
    ])
    for label, attempts in sorted(targets.items()):
        duration = f"{attempts.seconds:.3f}" if attempts.duration_known else "unavailable"
        label = label.replace("|", "\\|").replace("\n", " ")
        lines.append(f"| {label} | {attempts.local} | {attempts.remote} | {attempts.executed} | "
                     f"{attempts.retries} | {duration} | {attempts.status} |")
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("events", type=Path)
    arguments = parser.parse_args()
    if not arguments.events.is_file():
        print("## PostgreSQL test-result cache\n\nEvent file unavailable; cache reuse is unmeasured.")
        return
    with arguments.events.open() as events:
        print(summarize(read_events(events)), end="")


if __name__ == "__main__":
    main()

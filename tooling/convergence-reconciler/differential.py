#!/usr/bin/env python3
"""Compare recorded GitHub evidence with the frozen convergence reference."""
from __future__ import annotations

import argparse
import copy
import gzip
import json
from pathlib import Path
import subprocess
import sys
import tomllib

import reference

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "crates/convergence/fixtures"
POLICY = ROOT / "crates/convergence/examples/repository.toml"


def append_page(connection, page):
    if connection["totalCount"] != page["totalCount"]:
        raise ValueError("connection totalCount changed while paging")
    connection["nodes"].extend(copy.deepcopy(page["nodes"]))
    connection["pageInfo"] = copy.deepcopy(page["pageInfo"])


def assemble(responses):
    node = None
    for response in responses:
        data = response["response"]["data"]
        repository = data.get("repository", {})
        if "pullRequest" in repository:
            node = copy.deepcopy(repository["pullRequest"])
        if "object" in repository:
            node["commits"] = {"nodes": [{"commit": copy.deepcopy(repository["object"])}]}
        page_node = data.get("node")
        if page_node is None:
            continue
        if page_node["id"] == node["id"]:
            for kind in ("reviewThreads", "comments", "reviews", "reactions", "files"):
                if kind in page_node:
                    append_page(node[kind], page_node[kind])
        else:
            commit = node.get("commits", {}).get("nodes", [{}])[0].get("commit", {})
            rollup = commit.get("statusCheckRollup") or {}
            if page_node["id"] == rollup.get("id"):
                append_page(rollup["contexts"], page_node["contexts"])
            else:
                thread = next(t for t in node["reviewThreads"]["nodes"] if t["id"] == page_node["id"])
                append_page(thread["comments"], page_node["comments"])
    for kind in ("reviewThreads", "comments", "reviews", "reactions", "files"):
        complete(node[kind])
    for thread in node["reviewThreads"]["nodes"]:
        complete(thread["comments"])
    rollup = node["commits"]["nodes"][0]["commit"].get("statusCheckRollup")
    if rollup:
        complete(rollup["contexts"])
    return node


def complete(connection):
    if connection["pageInfo"]["hasNextPage"] or connection["totalCount"] != len(connection["nodes"]):
        raise ValueError("incomplete connection census")


class RecordedGitHub(reference.GitHubGraphQL):
    """Replay only responses contained in a recorded fixture."""

    def __init__(self, recording, current):
        super().__init__(recording["repository"], 1)
        self.recording = recording
        self.current = current

    def execute_rest(self, path):
        key = path.split("/compare/", 1)[1]
        value = self.recording["comparisons"][key]
        if value is None:
            raise reference.GitHubNotFoundError(key)
        return copy.deepcopy(value)

    def execute(self, query, variables):
        if "head" in variables and ":" in variables["head"]:
            path = variables["head"].split(":", 1)[1]
            return copy.deepcopy(self.recording["blobs"][path][0]["response"]["data"])
        node = copy.deepcopy(self.current)
        # The reference queries only root thumbs-up reactions.
        node["reactions"]["nodes"] = [r for r in node["reactions"]["nodes"] if r["content"] == "THUMBS_UP"]
        if "item0:" in query:
            return {"item0": node}
        if "state0:" in query:
            return {"state0": node}
        return {"node": node}


def reference_evaluation(recording):
    initial = assemble(recording["observations"][0])
    current = assemble(recording["observations"][-1])
    client = RecordedGitHub(recording, current)
    pr = reference.normalize_pull_request(initial)
    pr["_persisted_record"] = copy.deepcopy(recording.get("previous", {}))
    pr["review_threads"] = reference.normalize_review_threads(initial["reviewThreads"]["nodes"], pr["author_login"])
    pr["_review_thread_evidence"] = reference.review_thread_signature(pr["review_threads"])
    for thread in pr["review_threads"]:
        observed = pr["_persisted_record"].get("resolved_thread_observed_at", {}).get(thread.get("id"))
        if observed is not None:
            thread["resolutionObservedAt"] = observed
    pr["_thumbs_up_reactions"] = [r for r in initial["reactions"]["nodes"] if r["content"] == "THUMBS_UP"]
    client._validate_fixing_commits([pr])
    client._finalize_review_evidence([pr])
    client._restore_persisted_review_evidence([pr])
    client._load_review_exempt_status([pr])
    client._validate_review_waves([pr])
    client._load_renamed_paths([pr])
    client._load_planning_only_status([pr])
    client._load_base_ancestry([pr])
    client._finalize_check_inventory([pr])
    client.revalidate_for_decision(pr)
    return reference.evaluate_convergence(pr)


def fixtures():
    yield from sorted(FIXTURES.glob("pr-*.json*"))
    yield from sorted((FIXTURES / "mutations").glob("*.json*"))


def read_recording(path):
    with (gzip.open(path, "rt") if path.suffix == ".gz" else path.open()) as stream:
        value = json.load(stream)
    if "source" not in value:
        return value
    recording = read_recording(path.parent / value["source"])
    for mutation in value["mutations"]:
        parts = [part.replace("~1", "/").replace("~0", "~") for part in mutation["path"].split("/")[1:]]
        target = recording
        for part in parts[:-1]:
            target = target[int(part)] if isinstance(target, list) else target[part]
        key = int(parts[-1]) if isinstance(target, list) else parts[-1]
        target[key] = copy.deepcopy(mutation["value"])
    return recording


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/signalbox-converge")
    parser.add_argument("--write-expectations", action="store_true")
    parser.add_argument("fixtures", nargs="*", type=Path)
    args = parser.parse_args()
    cases = [path.resolve() for path in args.fixtures] or list(fixtures())
    if not cases:
        parser.error("fixture corpus is empty")
    failed = 0
    expectations = {}
    for path in cases:
        recording = read_recording(path)
        try:
            expected = reference_evaluation(recording)
        except (RuntimeError, ValueError, KeyError) as error:
            expected = {"error": str(error)}
        expectations[str(path.relative_to(FIXTURES))] = (
            {"error": expected["error"]} if "error" in expected else
            {"converged": expected["converged"], "reasons": expected["reasons"]}
        )
        completed = subprocess.run([str(args.binary), "evaluate", "--fixture", str(path), "--policy", str(POLICY)], capture_output=True, text=True, check=False)
        if "error" in expected:
            agrees = completed.returncode == 2
            actual = {"exit": completed.returncode, "stderr": completed.stderr.strip()}
        elif completed.returncode in (0, 1):
            actual = json.loads(completed.stdout)
            agrees = actual["converged"] == expected["converged"] and set(actual["reasons"]) == set(expected["reasons"])
        else:
            actual = {"exit": completed.returncode, "stderr": completed.stderr.strip()}
            agrees = False
        if not agrees:
            failed += 1
            print(json.dumps({"fixture": str(path.relative_to(ROOT)), "expected": expected, "actual": actual}, sort_keys=True))
    print(f"{len(cases)} fixtures, {failed} differences")
    if args.write_expectations and not failed:
        (FIXTURES / "expected.json").write_text(json.dumps(expectations, indent=2, sort_keys=True) + "\n")
    return int(failed != 0)


if __name__ == "__main__":
    sys.exit(main())

"""Retained context and terminal-result adapters for the bounded review judge."""

import base64
import hashlib
import json
import shutil
import socket
import subprocess


def prepare_workspace(workspace, session_id, checkout, head_sha, patch, context):
    """Provision the daemon's existing per-session root before submitting input."""
    root = workspace.with_name(workspace.name + ".sessions") / session_id
    root.mkdir(parents=True, exist_ok=True)

    def git(*arguments):
        return subprocess.run(["git", *map(str, arguments)], check=True,
                              capture_output=True, text=True).stdout.strip()

    if not (root / ".git").exists():
        git("init", root)
    head = root / "head"
    if not head.exists():
        git("clone", "--local", "--shared", "--no-checkout", checkout, head)
        git("-C", head, "checkout", "--detach", head_sha)
    if git("-C", head, "rev-parse", "HEAD") != head_sha:
        raise ValueError("agentic workspace differs from the reviewed head")
    git("-C", head, "diff", "--exit-code", "HEAD", "--")
    shutil.copyfile(patch, root / "change.patch")
    (root / "context.txt").write_text(context)
    return head


def retained_context(snapshot, *, pr, head_sha, finding_ids, source_thread_ids):
    if snapshot["pr"] != pr or snapshot["head_sha"] != head_sha:
        raise ValueError("review context differs from the pull request or head")
    context = {"pr": pr, "head_sha": head_sha}
    for kind, identity, subjects in (
        ("findings", "finding_id", finding_ids),
        ("threads", "thread_id", source_thread_ids),
    ):
        context[kind] = [{identity: item[identity], "author": item["author"],
                          "resolved": item["resolved"] if kind == "threads" else None,
                          "path": item["path"], "line": item["line"], "text": item["text"]}
                         for item in snapshot[kind] if item[identity] not in subjects]
    return context


def context_bytes_and_synopsis(context):
    encoded = json.dumps(context, ensure_ascii=False, separators=(",", ":")).encode()
    # Existing daemon blob_read decoded-byte ceiling, shared by the text tools.
    if len(encoded) > 512 * 1024:
        raise ValueError("retained review context exceeds the daemon text-tool read ceiling")
    digest = "sha256:" + hashlib.sha256(encoded).hexdigest()
    synopsis = []
    for kind, identity in (("findings", "finding_id"), ("threads", "thread_id")):
        for entry in context[kind]:
            text = " ".join(entry["text"].split())
            synopsis.append({**{key: value for key, value in entry.items() if key != "text"},
                             identity: digest + "#" + entry[identity],
                             "synopsis": text[:160], "text_truncated": len(text) > 160})
    return encoded, digest, synopsis


def upload_context(socket_path, encoded, digest):
    """Publish through the daemon's connection-local immutable-blob protocol."""
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(120)
        connection.connect(str(socket_path))
        with connection.makefile("rb") as reader:
            def exchange(ordinal, kind, **fields):
                connection.sendall((json.dumps({"version": 1, "request_id": str(ordinal),
                    "request": {"type": kind, **fields}}) + "\n").encode())
                line = reader.readline()
                if not line:
                    raise RuntimeError("daemon closed context upload")
                frame = json.loads(line)
                if frame["request_id"] != str(ordinal):
                    raise RuntimeError("uncorrelated context upload response")
                message = frame["message"]
                if message["type"] == "error":
                    raise RuntimeError(json.dumps(message))
                return message
            begun = exchange(1, "begin_blob_upload", expected_digest=digest,
                             expected_length_bytes=str(len(encoded)))
            if begun["type"] == "blob_upload_already_present":
                return begun
            exchange(2, "append_blob_upload", chunk=base64.b64encode(encoded).decode())
            committed = exchange(3, "commit_blob_upload")
            if committed.get("digest") != digest or committed.get("byte_length") != str(len(encoded)):
                raise RuntimeError("context upload did not commit the supplied bytes")
            return committed


def terminal_result(transcript, turn_id):
    state = next(item["state"] for item in transcript
                 if item["type"] == "transcript_turn" and item["turn_id"] == turn_id)
    if state["type"] != "completed":
        raise ValueError("judge turn has not completed")
    model_call_id = state["terminal_model_call_id"]
    terminal = sorted((item for item in transcript if item["type"] == "transcript_text_entry"
                       and item["entry"]["type"] == "assistant"
                       and item["entry"]["turn_id"] == turn_id
                       and item["entry"]["model_call_id"] == model_call_id),
                      key=lambda item: int(item["entry_index"]))
    if not terminal:
        raise ValueError("judge has no terminal assistant result")
    text = []
    for entry in terminal:
        chunks = sorted((item for item in transcript if item["type"] == "transcript_content"
                         and item["entry_index"] == entry["entry_index"]),
                        key=lambda item: int(item["fragment_index"]))
        if (not chunks or not chunks[-1]["final_fragment"]
                or [int(item["fragment_index"]) for item in chunks] != list(range(len(chunks)))):
            raise ValueError("terminal assistant result is incomplete")
        text.extend(item["content_fragment"] for item in chunks)
    return json.loads("".join(text)), {
        "source_session_id": terminal[-1]["source_session_id"],
        "model_call_id": model_call_id,
        "entries": [{key: entry[key] for key in ("entry_id", "entry_index")} for entry in terminal],
    }


def sum_usage(calls):
    """Sum reported axes while preserving any unknown component as unknown."""
    totals = {}
    for axis in ("input_tokens", "output_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"):
        values = [(call.get("usage") or {}).get(axis) for call in calls]
        totals[axis] = sum(int(value) for value in values) if values and all(value is not None for value in values) else None
    return totals

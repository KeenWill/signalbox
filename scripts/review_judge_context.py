"""Project retained sibling findings and review threads into judgment input."""

import json


def sibling_context(snapshot, *, pr, head_sha, finding_ids, source_thread_ids,
                    full_text_bytes):
    """Exclude subjects and labels, retaining full text for a small whole set.

    The caller supplies a retained snapshot for the reviewed head. This helper
    does not fetch a code host or infer when a comment existed.
    """
    if snapshot["pr"] != pr or snapshot["head_sha"] != head_sha:
        raise ValueError("sibling context differs from the pull request or review head")
    if full_text_bytes < 0:
        raise ValueError("full-text byte budget must be nonnegative")
    entries = []
    for kind, identity, subjects in (
        ("findings", "finding_id", finding_ids),
        ("threads", "thread_id", source_thread_ids),
    ):
        for item in snapshot[kind]:
            if item[identity] in subjects:
                continue
            entries.append({
                identity: item[identity], "author": item["author"],
                "resolved": item["resolved"] if kind == "threads" else None,
                "path": item["path"], "line": item["line"], "text": item["text"],
            })
    full_size = len(json.dumps(entries, ensure_ascii=False, separators=(",", ":")).encode())
    full = full_size <= full_text_bytes
    return {
        "pr": pr, "head_sha": head_sha, "full_text_bytes": full_size,
        "full_text_budget_bytes": full_text_bytes,
        "entries": [{**entry, "text": entry["text"] if full else entry["text"][:300],
                     "text_truncated": not full and len(entry["text"]) > 300}
                    for entry in entries],
    }

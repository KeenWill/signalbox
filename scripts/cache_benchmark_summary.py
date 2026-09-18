"""Summarize Bazel evidence; optionally retain raw Prometheus window samples.

Only Python's standard library is needed. The compact execution log and JSON
profile are retained for action-level analysis; BEP supplies runner counts.
gRPC field numbers follow Bazel's remote_execution_log.proto:
https://github.com/bazelbuild/bazel/blob/9.2.0/src/main/protobuf/remote_execution_log.proto
"""

import argparse
from collections import Counter, defaultdict
import csv
from datetime import datetime
import io
import json
import math
from pathlib import Path
import re
import urllib.parse
import urllib.request


NA = "NA"


def varint(stream):
    value = 0
    for shift in range(0, 70, 7):
        byte = stream.read(1)
        if not byte:
            raise ValueError("truncated protobuf varint")
        value |= (byte[0] & 127) << shift
        if byte[0] < 128:
            return value
    raise ValueError("invalid protobuf varint")


def fields(data):
    stream = io.BytesIO(data)
    result = defaultdict(list)
    while stream.tell() < len(data):
        key = varint(stream)
        number, wire = key >> 3, key & 7
        if not number:
            raise ValueError("invalid protobuf field")
        if wire == 0:
            value = varint(stream)
        elif wire in (1, 2, 5):
            size = varint(stream) if wire == 2 else {1: 8, 5: 4}[wire]
            value = stream.read(size)
            if len(value) != size:
                raise ValueError("truncated protobuf field")
        else:
            raise ValueError(f"unsupported protobuf wire type {wire}")
        result[number].append(value)
    return result


def first(message, number, default=0):
    return message.get(number, [default])[0]


def timestamp(data):
    parts = fields(data)
    return first(parts, 1) + first(parts, 2) / 1e9


def grpc_calls(path):
    with path.open("rb") as stream:
        while stream.peek(1):
            size = varint(stream)
            data = stream.read(size)
            if len(data) != size:
                raise ValueError("truncated gRPC log entry")
            entry = fields(data)
            method = first(entry, 3, b"").decode().rsplit("/", 1)[-1]
            if 5 not in entry or 6 not in entry:
                raise ValueError("gRPC timestamps missing")
            start, end = timestamp(entry[5][0]), timestamp(entry[6][0])
            if end < start:
                raise ValueError("gRPC end precedes start")
            call = {"method": method, "status": first(fields(first(entry, 2, b"")), 1),
                    "start": start, "end": end, "latency_ms": (end - start) * 1000}
            if method in ("Read", "Write"):
                details = fields(first(entry, 4, b""))
                kind = 5 if method == "Read" else 6
                if kind not in details:
                    raise ValueError(f"{method} details missing")
                payload = fields(details[kind][0])
                resources = ([first(fields(first(payload, 1, b"")), 1, b"")]
                             if method == "Read" else payload.get(1, []))
                resources = [value.decode() for value in resources if value]
                call.update(bytes=first(payload, 3), resources=resources)
                if len(resources) != 1:
                    raise ValueError(f"{method} resource name missing or ambiguous")
                resource = resources[0].split("/")
                call["blob_size"] = int(resource[-1])
                call["digest"] = resource[-2]
                call["compression"] = "zstd" if "compressed-blobs" in resource else "identity"
            yield call


def percentiles(values):
    ordered = sorted(values)
    return {f"p{p}_ms": ordered[math.ceil(len(ordered) * p / 100) - 1] if ordered else NA
            for p in (50, 95, 99)}


def summarize_grpc(path):
    methods = defaultdict(list)
    histograms = {"Read": Counter(), "Write": Counter()}
    sizes = {"Read": 0, "Write": 0}
    for call in grpc_calls(path):
        methods[call["method"]].append(call)
        if call["method"] in histograms:
            size = call["blob_size"]
            bucket = "zero" if size == 0 else str(size.bit_length() - 1)
            histograms[call["method"]][bucket] += 1
            sizes[call["method"]] += call["bytes"]
    if not methods:
        raise ValueError("gRPC log contains no completed RPCs")
    return {
        "bytes_down": sizes["Read"], "bytes_up": sizes["Write"],
        "latency": percentiles([c["latency_ms"] for calls in methods.values() for c in calls]),
        "methods": {name: {"count": len(calls),
                           "statuses": dict(Counter(c["status"] for c in calls)),
                           **percentiles([c["latency_ms"] for c in calls])}
                    for name, calls in methods.items()},
        "blob_histogram_log2": {name: dict(counts) for name, counts in histograms.items()},
    }


def summarize_bep(path):
    summary = None
    finished = False
    with path.open() as stream:
        for line in stream:
            event = json.loads(line)
            finished |= "finished" in event
            if "buildMetrics" in event:
                summary = event["buildMetrics"].get("actionSummary")
    if not finished or summary is None or "runnerCount" not in summary:
        raise ValueError("BEP missing buildFinished or action runner counts")
    runners = {r["name"]: int(r.get("count", 0)) for r in summary["runnerCount"]}
    if "total" not in runners:
        raise ValueError("BEP missing total runner count")
    # Bazel's total and individual runners can overlap; do not subtract hits
    # from total to infer executions. Internal bookkeeping is not a process.
    executed = sum(count for name, count in runners.items()
                   if name not in ("total", "internal", "remote cache hit", "disk cache hit"))
    return {"actions_total": runners["total"],
            "actions_cached": runners.get("remote cache hit", 0),
            "actions_executed": executed, "runners": runners}


def query(url, expression, when):
    params = urllib.parse.urlencode({"query": expression, "time": when})
    with urllib.request.urlopen(f"{url.rstrip('/')}/api/v1/query?{params}", timeout=30) as response:
        result = json.load(response)
    if result.get("status") != "success":
        raise ValueError(f"Prometheus query failed: {result}")
    return result


def collect_prometheus(metadata, url, namespace, cache_pods=None, cache_grpc_service=None):
    start, end = metadata["start"], metadata["end"]
    window = f"{math.floor((end - start) * 1000)}ms"
    evidence = {}

    def capture(name, expression):
        record = {"query": expression, "time": end}
        evidence[name] = record
        try:
            record["response"] = query(url, expression, end)
            return record["response"]["data"]["result"]
        except (OSError, ValueError, KeyError) as error:
            record["error"] = str(error)
            return []

    ns = json.dumps(namespace)
    pod = json.dumps(metadata["runner_pod"])
    cache_selector = f'namespace={ns}'
    if cache_pods:
        cache_selector += f',pod=~{json.dumps(cache_pods)}'
    if cache_grpc_service:
        capture("ac_cas", f'grpc_server_handled_total{{namespace={ns},service={json.dumps(cache_grpc_service)},'
                'grpc_service=~"build.bazel.remote.execution.v2.(ActionCache|ContentAddressableStorage)|google.bytestream.ByteStream"'
                f'}}[{window}]')
    else:
        capture("ac_cas", f'bazel_remote_incoming_requests_total{{{cache_selector}}}[{window}]')
    cpu = 'container_cpu_usage_seconds_total{job="kubelet",metrics_path="/metrics/cadvisor",container!="",container!="POD"'
    capture("cache_cpu_s", f'{cpu},{cache_selector}}}[{window}]')
    capture("runner_cpu_s", f'{cpu},namespace="github-arc-signalbox",pod={pod}}}[{window}]')
    capture("runner_placement", f'kube_pod_info{{namespace="github-arc-signalbox",pod={pod}}}')
    placements = capture("cache_placement", f'kube_pod_info{{{cache_selector}}}')
    nodes = {p["metric"]["node"] for p in placements}
    if nodes:
        node = json.dumps("|".join(re.escape(value) for value in sorted(nodes)))
        hosts = capture("cache_node", f'node_uname_info{{nodename=~{node}}}')
        instances = {host["metric"]["instance"] for host in hosts}
        if len(instances) == len(nodes):
            instance = json.dumps("|".join(re.escape(value) for value in sorted(instances)))
            for direction, metric in (("in", "receive"), ("out", "transmit")):
                capture(f"network_{direction}_bytes",
                        f'node_network_{metric}_bytes_total{{instance=~{instance},device="enp7s0np0"}}[{window}]')
    return evidence


def counter_deltas(record):
    if not record or "error" in record:
        raise ValueError(record.get("error", "query unavailable") if record else "query unavailable")
    result = record["response"]["data"]["result"]
    if not result:
        raise ValueError("no matching Prometheus series")
    deltas = []
    for series in result:
        values = series.get("values", [])
        if len(values) < 2:
            raise ValueError("fewer than two in-window counter samples")
        numbers = [float(sample[1]) for sample in values]
        if not all(math.isfinite(n) for n in numbers):
            raise ValueError("nonfinite counter sample")
        if any(b < a for a, b in zip(numbers, numbers[1:])):
            raise ValueError("counter reset; exact delta unavailable")
        deltas.append((series["metric"], numbers[-1] - numbers[0]))
    return deltas


def summarize(directory):
    row = {}
    details = {"missing": {}, "scope": {
        "wall_s": "Bazel command wall time, excluding setup, help and info",
        "job_wall_s": "complete GitHub job duration from job.json; shared by repetitions in one job",
        "actions": "BEP runner counts; total and cached counts may overlap; executed excludes internal bookkeeping",
        "bytes": "ByteStream payload, includes repository downloads and diagnostic uploads; excludes framing, AC metadata and origin downloads",
        "latency": "nearest-rank completed RPC duration, includes misses and failures",
        "histogram": "per Read/Write RPC requested logical blob bytes; bucket k is [2^k, 2^(k+1)); zero is separate",
        "prometheus": "shared service/node counters, last minus first actual in-window scrape; no extrapolation",
        "cpu_per_gib": "shared cache CPU delta / client download GiB; not client-attributable CPU",
    }}

    def missing(names, error):
        for name in names:
            row[name] = NA
            details["missing"][name] = str(error)

    try:
        metadata = json.loads((directory / "run.json").read_text())
        details["run"] = metadata
        for name in ("label", "mode", "wall_s", "exit_code"):
            if name in metadata:
                row[name] = metadata[name]
            else:
                missing([name], "run metadata field missing (interrupted run)")
    except (OSError, ValueError) as error:
        missing(["label", "mode", "wall_s", "exit_code"], error)
    try:
        job = json.loads((directory / "job.json").read_text())
        row["job_wall_s"] = (datetime.fromisoformat(job["completed_at"])
                             - datetime.fromisoformat(job["started_at"])).total_seconds()
    except (OSError, ValueError, KeyError, TypeError) as error:
        missing(["job_wall_s"], error)
    try:
        bep = summarize_bep(directory / "bep.jsonl")
        details["bep"] = bep
        row.update({name: value for name, value in bep.items() if name != "runners"})
    except (OSError, ValueError, KeyError) as error:
        missing(["actions_total", "actions_cached", "actions_executed"], error)
    try:
        grpc = summarize_grpc(directory / "grpc.bin")
        details["grpc"] = grpc
        row.update(bytes_down=grpc["bytes_down"], bytes_up=grpc["bytes_up"], **grpc["latency"])
    except (OSError, ValueError, KeyError, IndexError) as error:
        missing(["bytes_down", "bytes_up", "p50_ms", "p95_ms", "p99_ms"], error)
        details["blob_histogram_log2"] = {"value": NA, "reason": str(error)}
    try:
        prom = json.loads((directory / "prometheus.json").read_text())
    except (OSError, ValueError):
        prom = {}
    for name in ("cache_cpu_s", "runner_cpu_s", "network_in_bytes", "network_out_bytes", "ac_cas"):
        try:
            if "collection_error" in prom:
                raise ValueError(prom["collection_error"])
            deltas = counter_deltas(prom.get(name))
            if name == "ac_cas":
                counts = Counter()
                for labels, delta in deltas:
                    if "grpc_service" in labels:
                        kind = "ac" if labels["grpc_service"].endswith(".ActionCache") else "cas"
                        key = (kind, labels["grpc_method"], labels["grpc_code"])
                    else:
                        key = tuple(labels[k] for k in ("kind", "method", "status"))
                    counts["/".join(key)] += delta
                row[name] = dict(counts)
            else:
                row[name] = sum(delta for _, delta in deltas)
        except (ValueError, KeyError) as error:
            missing([name], error)
    if row["cache_cpu_s"] != NA and row["bytes_down"] != NA and row["bytes_down"] > 0:
        row["cache_cpu_s_per_client_gib"] = row["cache_cpu_s"] / (row["bytes_down"] / 2**30)
    else:
        missing(["cache_cpu_s_per_client_gib"], "cache CPU or positive downloaded bytes unavailable")
    for name in ("execution.bin.zst", "profile.json.gz", "bazel-info.txt", "flag-help.txt"):
        if not (directory / name).is_file() or not (directory / name).stat().st_size:
            details["missing"][name] = "evidence file missing or empty"
    details["row"] = row
    return details


def write_summary(directory, details):
    (directory / "summary.json").write_text(json.dumps(details, indent=2) + "\n")
    with (directory / "summary.tsv").open("w", newline="") as stream:
        writer = csv.writer(stream, delimiter="\t")
        writer.writerow(details["row"])
        writer.writerow(json.dumps(value, sort_keys=True) if isinstance(value, dict) else value
                        for value in details["row"].values())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--prometheus")
    parser.add_argument("--cache-namespace", help="Namespace of the measured endpoint; required with --prometheus")
    parser.add_argument("--cache-pods", help="Pod name regex for cache CPU and placement; includes all measured tiers")
    parser.add_argument("--cache-grpc-service", help="Frontend service exposing grpc_server_handled_total instead of bazel-remote counters")
    args = parser.parse_args()
    if args.prometheus:
        if not args.cache_namespace:
            parser.error("--prometheus requires --cache-namespace")
        try:
            metadata = json.loads((args.directory / "run.json").read_text())
            evidence = collect_prometheus(metadata, args.prometheus, args.cache_namespace,
                                          args.cache_pods, args.cache_grpc_service)
        except (OSError, ValueError, KeyError) as error:
            evidence = {"collection_error": f"Prometheus collection unavailable: {error}"}
        (args.directory / "prometheus.json").write_text(json.dumps(evidence, indent=2) + "\n")
    write_summary(args.directory, summarize(args.directory))
    print((args.directory / "summary.tsv").read_text(), end="")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Measure search latency through the public SSE endpoint.

The server and corpus are supplied by the caller; scripts/bench/search.sh starts a
local instance. Results include every request so percentiles can be recomputed.
"""

import argparse
import concurrent.futures
import json
import math
import statistics
import time
import urllib.parse
import urllib.request
from datetime import datetime, timezone


DEFAULT_QUERIES = {
    "selective": "func main index:only",
    "common": "reconcile index:only",
    "regex": "func\\s+main patterntype:regexp index:only",
    "unindexed": "func main index:no",
}


def percentile(values, percent):
    ordered = sorted(values)
    rank = math.ceil(percent / 100 * len(ordered)) - 1
    return ordered[max(0, rank)]


def measure(base, query, timeout, pattern_type, allow_display_limit, collect=None,
            strict=True):
    """One search; `collect` (a callable) receives each `matches` event's list in order.
    Without `strict`, search errors and skipped reasons are returned instead of raised."""
    url = base.rstrip("/") + "/.api/search/stream?" + urllib.parse.urlencode(
        {"q": query, "v": "V3", "t": pattern_type, "cm": "t"}
    )
    start = time.perf_counter()
    first_event = first_match = done = None
    matches = 0
    skipped = []
    errors = []
    event = None
    data = []
    with urllib.request.urlopen(url, timeout=timeout) as response:
        if response.status != 200:
            raise RuntimeError(f"HTTP {response.status}")
        if not response.headers.get("Content-Type", "").startswith("text/event-stream"):
            raise RuntimeError("response is not an SSE stream")
        for raw in response:
            line = raw.decode("utf-8").rstrip("\r\n")
            if line.startswith(":"):
                continue
            if line.startswith("event:"):
                event = line[6:].strip()
            elif line.startswith("data:"):
                data.append(line[5:].lstrip())
            elif line == "" and event:
                elapsed = (time.perf_counter() - start) * 1000
                if first_event is None:
                    first_event = elapsed
                payload = json.loads("\n".join(data)) if data else None
                if event == "matches":
                    matches += len(payload)
                    if collect is not None:
                        collect(payload)
                    if payload and first_match is None:
                        first_match = elapsed
                elif event == "progress" and payload:
                    skipped = payload.get("skipped") or []
                elif event in ("error", "alert"):
                    errors.append({"event": event, "data": payload})
                elif event == "done":
                    done = elapsed
                    break
                event, data = None, []
    if done is None:
        raise RuntimeError("SSE stream ended without done")
    unexpected = [item for item in skipped if
                  item.get("reason") != "display" or not allow_display_limit]
    if (errors or unexpected) and strict:
        raise RuntimeError(f"search reported errors={errors}, skipped={unexpected}")
    result = {"first_event_ms": first_event, "first_match_ms": first_match,
              "done_ms": done, "matches": matches,
              "display_limit_hit": any(item.get("reason") == "display"
                                       for item in skipped)}
    if not strict:
        result["errors"] = errors
        result["skipped"] = sorted({item.get("reason", "") for item in skipped})
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="http://127.0.0.1:7090")
    parser.add_argument("--queries", help="JSON object mapping case names to query strings")
    parser.add_argument("--repeats", type=int, default=20)
    parser.add_argument("--warmups", type=int, default=3)
    parser.add_argument("--concurrency", type=int, default=1)
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--pattern-type", default="keyword",
                        choices=("keyword", "standard", "regexp"),
                        help="default parser mode (the UI defaults to keyword)")
    parser.add_argument("--allow-display-limit", action="store_true",
                        help="allow the intentional count/display limit in progress")
    parser.add_argument("--output", help="write full JSON results to this path")
    args = parser.parse_args()
    if min(args.repeats, args.concurrency) < 1 or args.warmups < 0:
        parser.error("repeats and concurrency must be positive; warmups must be nonnegative")
    queries = json.load(open(args.queries, encoding="utf-8")) if args.queries else DEFAULT_QUERIES
    if not isinstance(queries, dict) or not queries or not all(
        isinstance(k, str) and isinstance(v, str) and v for k, v in queries.items()
    ):
        parser.error("queries must be a nonempty JSON object of nonempty strings")

    results = {}
    for name, query in queries.items():
        for _ in range(args.warmups):
            measure(args.base, query, args.timeout, args.pattern_type,
                    args.allow_display_limit)
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
            samples = list(pool.map(
                lambda _: measure(args.base, query, args.timeout, args.pattern_type,
                                  args.allow_display_limit),
                range(args.repeats)
            ))
        if len({sample["matches"] for sample in samples}) != 1:
            raise RuntimeError(f"{name}: match count changed across identical requests")
        results[name] = {"query": query, "samples": samples}
        for field in ("first_event_ms", "first_match_ms", "done_ms"):
            values = [sample[field] for sample in samples if sample[field] is not None]
            if values:
                print(f"{name:12} {field:17} p50={statistics.median(values):7.1f} "
                      f"p95={percentile(values, 95):7.1f} p99={percentile(values, 99):7.1f} "
                      f"n={len(values)}")
        print(f"{'':12} matches={samples[0]['matches']}")
    output = {"utc": datetime.now(timezone.utc).isoformat(), "base": args.base,
              "warmups": args.warmups, "repeats": args.repeats,
              "concurrency": args.concurrency, "cases": results}
    output["pattern_type"] = args.pattern_type
    output["chunk_matches"] = True
    output["allow_display_limit"] = args.allow_display_limit
    if args.output:
        with open(args.output, "w", encoding="utf-8") as file:
            json.dump(output, file, indent=2)
            file.write("\n")


if __name__ == "__main__":
    main()

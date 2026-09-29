"""Bench driver: replay a seed-pinned op stream against ltmrs (mirror of
tools/bench_against_lemma.mjs, which drives upstream).

Comparison contract (same boundaries):
- Identical op mapping: put -> memory_add, get -> memory_read (mapped id
  or never-written key), search -> semantic_search {query, topK}.
- One op outstanding at a time; per-op wall time covers the stdio round
  trip including per-op work (conflict scans, suggestions, projection).
- Same transcript JSONL shape: {seq, kind, elapsed_ms, ok, detail}.
- keyToId from `[id]` in add responses, same regex as the upstream driver.
- get failure counts are NOT comparable across sides (upstream read-miss
  errors; ltmrs records miss as ok found=0) — hit latencies are.

Usage:
  HOME=<scratch> python3 tools/bench_against_ltmrs.py \
    --ops reports/wp12-lemma-01/ops.jsonl \
    --out /tmp/diff-rt/out/ltmrs-100.jsonl [--limit N]
"""
import json
import os
import re
import subprocess
import sys
import time

BIN = "/home/nvand/Workspace/Dev/ltmrs/target/release/ltmrs"


def parse_args(argv):
    args = {}
    i = 0
    while i < len(argv):
        assert argv[i].startswith("--"), argv[i]
        args[argv[i][2:]] = argv[i + 1]
        i += 2
    return args


def main():
    args = parse_args(sys.argv[1:])
    ops = [json.loads(l) for l in open(args["ops"]) if l.strip()]
    limit = int(args.get("limit", len(ops)))
    ops = ops[:limit]
    out_path = args["out"]

    proc = subprocess.Popen(
        [BIN], env={**os.environ},
        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL, text=True, bufsize=1)

    def send(method, params, mid=None):
        msg = {"jsonrpc": "2.0", "method": method}
        if mid is not None:
            msg["id"] = mid
        if params is not None:
            msg["params"] = params
        proc.stdin.write(json.dumps(msg) + "\n")
        proc.stdin.flush()

    def recv(want, timeout_ms=30000):
        end = time.time() + timeout_ms / 1000
        while True:
            line = proc.stdout.readline().strip()
            if line:
                m = json.loads(line)
                if m.get("id") == want:
                    return m
            assert time.time() < end, f"request {want} timed out"

    t0 = time.time()
    send("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                        "clientInfo": {"name": "bench", "version": "0"}},
         mid=0)
    init = recv(0)
    assert init.get("result"), "initialize failed"
    send("notifications/initialized", None)
    print(f"initialize: {round((time.time()-t0)*1000,1)}ms", flush=True)

    key_to_id = {}
    results = []
    mid = 1
    for op in ops:
        unmapped = False
        if op["kind"] == "put":
            call = {"name": "memory_add",
                    "arguments": {"title": op.get("title") or f"Workload {op['key']}",
                                  "fragment": op["fragment"]}}
        elif op["kind"] == "get":
            mapped = key_to_id.get(str(op["key"]))
            unmapped = mapped is None
            call = {"name": "memory_read",
                    "arguments": {"id": mapped or f"never-written-{op['key']}"}}
        else:
            call = {"name": "semantic_search",
                    "arguments": {"query": op["query"], "topK": op.get("top_k", 10)}}
        t1 = time.time()
        send("tools/call", {"name": call["name"], "arguments": call["arguments"]},
             mid=mid)
        resp = recv(mid)
        elapsed = (time.time() - t1) * 1000
        mid += 1
        result = resp.get("result", {})
        ok = not result.get("isError", False)
        text = ""
        for c in result.get("content", []):
            if isinstance(c, dict) and c.get("type") == "text":
                text += c.get("text", "")
        detail = text.replace("\n", " | ")[:300] or json.dumps(result)[:300]
        if op["kind"] == "put" and ok:
            m = re.search(r"\[([A-Za-z0-9]+)\]", detail)
            if m:
                key_to_id[str(op["key"])] = m.group(1)
        if op["kind"] == "get" and unmapped:
            detail = f"unmapped get (no put observed for key): {detail}"
        results.append({"seq": op["seq"], "kind": op["kind"],
                        "elapsed_ms": elapsed, "ok": ok, "detail": detail})
        if len(results) % 100 == 0:
            print(f"  {len(results)}/{len(ops)} ops", flush=True)
    os.makedirs(os.path.dirname(os.path.abspath(out_path)), exist_ok=True)
    with open(out_path, "w") as f:
        for r in results:
            f.write(json.dumps(r) + "\n")
    print(f"wrote {len(results)} records to {out_path}", flush=True)
    proc.stdin.close()
    proc.wait(timeout=60)


if __name__ == "__main__":
    main()

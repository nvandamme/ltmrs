"""Live-agent quality wave (manual evidence, not suite).

Design: an independent LLM rater judges whether ltmrs's top hybrid hit
answers heldout calibration queries (LLM-as-judge). Rater, prompt and
sampling are pinned below; verdicts recorded verbatim.

- Rater: local LM Studio endpoint, model
  qwen3.8-27b-efficientthink-simpo-lynnstyle@q5, temperature 0.
- Sample: 20 heldout cases, RNG seed 7 (fixed order).
- Store: 90 heldout memories indexed in PROBE_HOME (see seed step).
- Verdict parse: last non-empty content line, YES/NO else UNCERTAIN.
- Output: reports/release-01/agent-wave.json

Usage:
  PROBE_HOME=/tmp/agent-wave/home python3 tools/agent_quality_wave.py
"""
import json
import os
import random
import subprocess
import sys
import time
import urllib.request

BIN = "/home/nvand/Workspace/Dev/ltmrs/target/release/ltmrs"
HOME = os.environ["PROBE_HOME"]
RATER_URL = os.environ.get("RATER_URL", "http://localhost:1234/v1/chat/completions")
RATER_MODEL = os.environ.get(
    "RATER_MODEL", "qwen3.8-27b-efficientthink-simpo-lynnstyle@q5")
N_CASES = int(os.environ.get("WAVE_CASES", "20"))
OUT = "/home/nvand/Workspace/Dev/ltmrs/reports/release-01/agent-wave.json"

SYSTEM = ("You are a search relevance judge. Bridge synonyms: a query may "
          "use different words than the memory (e.g. air steadiness = "
          "seeing, fermented culture = levain). "
          "Example 1: Query: Why upgrade air steadiness first? / Memory: "
          "Doubling seeing beats any upgrade to dew-shield. Answer: YES. "
          "Example 2: Query: How do sappers get past a murder-hole? / "
          "Memory: Commanders starved murder-hole rather than storm "
          "counterweight. Answer: NO (starving is not sapping past). "
          "Reply with exactly one word on the last line: YES or NO.")


def rate(query, title, fragment):
    body = json.dumps({
        "model": RATER_MODEL,
        "messages": [
            {"role": "system", "content": SYSTEM},
            {"role": "user",
             "content": f"Query: {query}\nRetrieved memory title: {title}\n"
                        f"Retrieved memory text: {fragment}\n"
                        "Does it answer the query?"},
        ],
        "temperature": 0,
        "max_tokens": 512,
    }).encode()
    req = urllib.request.Request(
        RATER_URL, data=body, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=600) as r:
        resp = json.load(r)
    msg = resp["choices"][0]["message"]
    content = (msg.get("content") or "").strip()
    lines = [ln.strip().upper() for ln in content.splitlines() if ln.strip()]
    verdict = lines[-1] if lines else ""
    if verdict.startswith("YES"):
        return "YES", content
    if verdict.startswith("NO"):
        return "NO", content
    return "UNCERTAIN", content


def main():
    fx = json.load(open(
        "/home/nvand/Workspace/Dev/ltmrs/experiments/quality/retrieval-calibration.json"))
    held = [c for c in fx["cases"] if c["split"] == "heldout"]
    rng = random.Random(7)
    sample = rng.sample(held, N_CASES)
    mem_by_key = {m["key"]: m for m in fx["memories"]}

    proc = subprocess.Popen(
        [BIN], env={**os.environ, "HOME": HOME},
        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL, text=True, bufsize=1)

    def send(m):
        proc.stdin.write(json.dumps(m) + "\n")
        proc.stdin.flush()

    def recv(want, dl=120):
        end = time.time() + dl
        while True:
            line = proc.stdout.readline().strip()
            if line:
                m = json.loads(line)
                if m.get("id") == want:
                    return m
            assert time.time() < end, f"timeout@{want}"

    def call(mid, name, args):
        send({"jsonrpc": "2.0", "id": mid, "method": "tools/call",
              "params": {"name": name, "arguments": args}})
        return recv(mid)

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
          "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                     "clientInfo": {"name": "wave", "version": "0.0"}}})
    recv(1)
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    rows = []
    for i, case in enumerate(sample):
        s = call(10 + i, "semantic_search",
                 {"query": case["query"], "explain": True, "topK": 3})
        sc = s["result"]["structuredContent"]
        top = sc["results"][0] if sc["results"] else None
        if top is None:
            rows.append({"query": case["query"], "targets": case["targets"],
                         "verdict": "NO-RESULT", "top": None})
            continue
        verdict, raw = rate(case["query"], top["title"],
                            top.get("fragment_preview", ""))
        rows.append({"query": case["query"], "targets": case["targets"],
                     "top_title": top["title"], "top_score": top["score"],
                     "mode": sc["explanation"]["mode"],
                     "verdict": verdict, "rater_raw_tail": raw[-300:]})
        print(f"[{i+1}/{len(sample)}] {verdict} "
              f"({top['title'][:40]} {top['score']:.3f})", flush=True)
    proc.stdin.close()
    proc.wait(timeout=120)

    yes = sum(1 for r in rows if r["verdict"] == "YES")
    no = sum(1 for r in rows if r["verdict"] == "NO")
    unc = sum(1 for r in rows if r["verdict"] not in ("YES", "NO"))
    report = {"rater_model": RATER_MODEL, "temperature": 0,
              "sample_seed": 7, "n": len(rows),
              "yes": yes, "no": no, "uncertain_or_empty": unc,
              "agreement_rate": yes / max(1, yes + no),
              "rows": rows}
    json.dump(report, open(OUT, "w"), indent=1)
    print(f"YES={yes} NO={no} UNCERTAIN/EMPTY={unc} "
          f"agreement={report['agreement_rate']:.3f} -> {OUT}")


if __name__ == "__main__":
    main()

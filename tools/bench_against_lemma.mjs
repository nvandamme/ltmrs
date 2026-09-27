#!/usr/bin/env node
// Development tool: run an exported seed-pinned op stream (see
// src/bench/experiment.rs `export_workload_ops`) against the pinned upstream
// Lemma MCP server and record per-op timings.
//
// Usage:
//   node tools/bench_against_lemma.mjs --server <dist/index.js> --ops <ops.jsonl> --home <scratch-dir> --out <transcript.jsonl> [--limit N]
//
// Comparison contract (T-BENCH-01, declared boundaries):
// - The op stream is identical to the ltmrs run (same seed, same JSONL).
//   Put payloads and search queries ride the file; nothing is re-derived.
// - One op outstanding at a time (matches the single-threaded ltmrs legs).
// - Timing covers the stdio round trip per op, including whatever per-op
//   work upstream does (conflict scans, suggestions). That work is part of
//   the measured system, not overhead to subtract.
// - KNOWN SEMANTIC DIFFERENCE: upstream read-miss surfaces as an MCP error
//   ("not found"); ltmrs records miss as ok found=0. `ok` below mirrors the
//   upstream response (isError => ok=false), so get failure counts are NOT
//   comparable across sides — hit latencies are.
// - The child runs with HOME=<home> (plus XDG_* pinned beneath it) so the
//   bench store never touches a live knowledge base. Refuses to run when
//   --home resolves (symlinks followed) to the real $HOME.
// - Every request carries a 30 s timeout: a hung or dead server fails the
//   run loudly instead of hanging forever.

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const REQUEST_TIMEOUT_MS = 30_000;

function parseArgs(argv) {
  const args = {};
  for (let i = 2; i < argv.length; i++) {
    const flag = argv[i];
    if (!flag.startsWith("--")) {
      console.error(`unexpected positional argument: ${flag}`);
      process.exit(2);
    }
    const value = argv[++i];
    if (value === undefined || value.startsWith("--")) {
      console.error(`flag ${flag} requires a value`);
      process.exit(2);
    }
    args[flag.slice(2)] = value;
  }
  return args;
}

function fail(child, message) {
  console.error(message);
  try {
    child.kill("SIGKILL");
  } catch {
    /* already gone */
  }
  process.exit(1);
}

const args = parseArgs(process.argv);
const serverPath = args.server;
const opsPath = args.ops;
const home = args.home;
const outPath = args.out;
if (!serverPath || !opsPath || !home || !outPath) {
  console.error(
    "usage: bench_against_lemma.mjs --server <dist/index.js> --ops <ops.jsonl> --home <scratch> --out <transcript.jsonl> [--limit N]"
  );
  process.exit(2);
}
let limit = Infinity;
if (args.limit !== undefined) {
  limit = parseInt(args.limit, 10);
  if (!Number.isFinite(limit) || limit <= 0) {
    console.error(`--limit must be a positive integer, got: ${args.limit}`);
    process.exit(2);
  }
}
// Symlinks followed on both sides: --home /tmp/link when link -> $HOME
// must not pass the guard.
fs.mkdirSync(path.resolve(home), { recursive: true });
const realHome = fs.realpathSync(path.resolve(home));
if (realHome === fs.realpathSync(os.homedir())) {
  console.error("refusing to bench against the live $HOME knowledge base");
  process.exit(2);
}

let ops;
try {
  ops = fs
    .readFileSync(opsPath, "utf8")
    .split("\n")
    .filter((l) => l.trim().length > 0)
    .map((l) => JSON.parse(l))
    .slice(0, limit);
} catch (e) {
  console.error(`cannot read ops file ${opsPath}: ${e.message}`);
  process.exit(2);
}
if (ops.length === 0) {
  console.error("no ops to run (empty stream or bad --limit)");
  process.exit(2);
}
for (const op of ops) {
  if (!["put", "get", "search"].includes(op.kind)) {
    console.error(`unknown op kind: ${JSON.stringify(op.kind)}`);
    process.exit(2);
  }
}

const child = spawn("node", [serverPath], {
  env: {
    ...process.env,
    HOME: realHome,
    XDG_DATA_HOME: path.join(realHome, ".local", "share"),
    XDG_CONFIG_HOME: path.join(realHome, ".config"),
    XDG_CACHE_HOME: path.join(realHome, ".cache"),
  },
  stdio: ["pipe", "pipe", "inherit"],
});
child.stdin.setDefaultEncoding("utf8");

let nextId = 1;
const pending = new Map();
let buffer = "";
const results = [];
const keyToId = new Map();

function rejectAll(error) {
  for (const { reject, timer } of pending.values()) {
    clearTimeout(timer);
    reject(error);
  }
  pending.clear();
}

child.on("error", (e) => rejectAll(new Error(`server process error: ${e.message}`)));
child.on("close", (code) => {
  if (pending.size > 0) rejectAll(new Error(`server closed (code ${code}) with ${pending.size} ops outstanding`));
});
child.stdin.on("error", (e) => rejectAll(new Error(`server stdin error: ${e.message}`)));

function send(method, params) {
  return new Promise((resolve, reject) => {
    if (params === undefined) {
      // Notification: no id, no response expected.
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method }) + "\n");
      resolve({ elapsedMs: 0, response: null });
      return;
    }
    const id = nextId++;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`request ${id} (${method}) timed out after ${REQUEST_TIMEOUT_MS} ms`));
    }, REQUEST_TIMEOUT_MS);
    pending.set(id, { resolve, reject, timer, at: performance.now() });
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  });
}

child.stdout.setEncoding("utf8");
child.stdout.on("data", (chunk) => {
  buffer += chunk;
  let nl;
  while ((nl = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, nl).trim();
    buffer = buffer.slice(nl + 1);
    if (!line) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    if (msg.id !== undefined && pending.has(msg.id)) {
      const { resolve, timer, at } = pending.get(msg.id);
      pending.delete(msg.id);
      clearTimeout(timer);
      resolve({ elapsedMs: performance.now() - at, response: msg });
    }
  }
});

function summarizeCall(response) {
  if (!response) return { ok: false, detail: "no response" };
  if (response.error) {
    return { ok: false, detail: `rpc error: ${response.error.message || "?"}` };
  }
  const result = response.result || {};
  const text =
    (result.content || [])
      .map((c) => c.text || "")
      .join("\n")
      .slice(0, 300) || JSON.stringify(result).slice(0, 300);
  return { ok: !result.isError, detail: text.replace(/\n/g, " | ") };
}

async function main() {
  const init = await send("initialize", {
    protocolVersion: "2025-06-18",
    capabilities: {},
    clientInfo: { name: "bench", version: "0" },
  });
  if (!init.response || !init.response.result) {
    fail(child, "initialize failed");
  }
  await send("notifications/initialized");

  let done = 0;
  for (const op of ops) {
    let call;
    let unmappedGet = false;
    if (op.kind === "put") {
      call = {
        name: "memory_add",
        arguments: { title: op.title || `Workload ${op.key}`, fragment: op.fragment },
      };
    } else if (op.kind === "get") {
      const mapped = keyToId.get(String(op.key));
      unmappedGet = mapped === undefined;
      call = {
        name: "memory_read",
        arguments: { id: mapped || `never-written-${op.key}` },
      };
    } else {
      call = {
        name: "semantic_search",
        arguments: { query: op.query, topK: op.top_k },
      };
    }
    let outcome;
    try {
      outcome = await send("tools/call", { name: call.name, arguments: call.arguments });
    } catch (e) {
      fail(child, `op ${done} (${op.kind}) failed: ${e.message}`);
    }
    const { ok, detail } = summarizeCall(outcome.response);
    if (op.kind === "put" && ok) {
      const m = detail.match(/\[([A-Za-z0-9]+)\]/);
      if (m) keyToId.set(String(op.key), m[1]);
    }
    results.push({
      seq: op.seq,
      kind: op.kind,
      elapsed_ms: outcome.elapsedMs,
      ok,
      detail: unmappedGet ? `unmapped get (no put observed for key): ${detail}` : detail,
    });
    done++;
    if (done % 100 === 0) console.error(`  ${done}/${ops.length} ops`);
  }

  fs.mkdirSync(path.dirname(path.resolve(outPath)), { recursive: true });
  fs.writeFileSync(outPath, results.map((r) => JSON.stringify(r)).join("\n") + "\n");
  console.error(`wrote ${results.length} records to ${outPath}`);
  child.kill();
}

main().catch((e) => fail(child, `bench driver failed: ${e.message}`));

#!/usr/bin/env node
// Development-only utility: capture the live MCP wire transcript of the pinned
// Lemma upstream (0.21.0) running in an isolated sandbox HOME.
//
// Usage:
//   node tools/capture_lemma.mjs --repo <lemma-checkout> --home <sandbox-home> --out <baseline-dir>
//
// Emits: wire/initialize.json, wire/tools_list.json, wire/tools_call_*.json,
//        wire/notifications.json, wire/transcript.jsonl
//
// Provenance: records commit, versions, protocol versions and timestamps so the
// capture is reproducible. Failures are recorded as not_captured, never faked.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

function parseArgs(argv) {
  const args = {};
  for (let i = 2; i < argv.length; i++) {
    if (argv[i].startsWith("--")) args[argv[i].slice(2)] = argv[++i];
  }
  return args;
}

const args = parseArgs(process.argv);
const repo = args.repo;
const sandboxHome = args.home;
const outDir = args.out;
if (!repo || !sandboxHome || !outDir) {
  console.error("usage: capture_lemma.mjs --repo <dir> --home <dir> --out <dir>");
  process.exit(2);
}

const wireDir = path.join(outDir, "wire");
fs.mkdirSync(wireDir, { recursive: true });

// Provenance
function gitIn(dir, cmd) {
  try {
    return execSync(cmd, { cwd: dir, stdio: "pipe" }).toString().trim();
  } catch {
    return "unknown";
  }
}
function readJsonField(p, key) {
  try {
    return JSON.parse(fs.readFileSync(p, "utf8"))[key];
  } catch {
    return "unknown";
  }
}
const provenance = {
  captured_at: new Date().toISOString(),
  lemma_commit: gitIn(repo, "git rev-parse HEAD"),
  lemma_version: "0.21.0",
  node_version: process.version,
  mcp_sdk_version: readJsonField(path.join(repo, "node_modules/@modelcontextprotocol/sdk/package.json"), "version"),
  sandbox_home: sandboxHome,
};
fs.writeFileSync(path.join(outDir, "provenance.json"), JSON.stringify(provenance, null, 2));

const serverArgs = [path.join(repo, "dist/index.js")];
const child = spawn(process.execPath, serverArgs, {
  env: { ...process.env, HOME: sandboxHome, LEMMA_LOG_STDERR: "0" },
  stdio: ["pipe", "pipe", "pipe"],
});

const transcript = [];
let buffer = "";
const pending = new Map(); // id -> resolve

child.stdout.on("data", (d) => {
  buffer += d.toString();
  let idx;
  while ((idx = buffer.indexOf("\n")) !== -1) {
    const line = buffer.slice(0, idx).trim();
    buffer = buffer.slice(idx + 1);
    if (!line) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      transcript.push({ raw_nonjson: line });
      continue;
    }
    transcript.push(msg);
    if (msg.id !== undefined && pending.has(msg.id)) {
      pending.get(msg.id)(msg);
      pending.delete(msg.id);
    }
  }
});
let stderrBuf = "";
child.stderr.on("data", (d) => (stderrBuf += d.toString()));

function send(method, params, id) {
  const m = id === undefined ? { jsonrpc: "2.0", method, params } : { jsonrpc: "2.0", method, params, id };
  child.stdin.write(JSON.stringify(m) + "\n");
}
function call(method, params) {
  const id = Math.floor(Math.random() * 1e9);
  return new Promise((resolve) => {
    pending.set(id, resolve);
    send(method, params, id);
    setTimeout(() => {
      if (pending.has(id)) {
        pending.delete(id);
        resolve({ __timeout: true, method });
      }
    }, 8000);
  });
}

const results = {};
async function main() {
  // 1. initialize
  const init = await call("initialize", {
    protocolVersion: "2025-11-25",
    capabilities: {},
    clientInfo: { name: "ltmrs-capture", version: "0.1.0" },
  });
  results.initialize = init;
  send("notifications/initialized", {});
  await new Promise((r) => setTimeout(r, 300));

  // 2. tools/list
  const toolsList = await call("tools/list", {});
  results.tools_list = toolsList;

  // 3. A few representative tools/call (read-only, against empty sandbox store)
  const calls = [
    ["memory_read", {}],
    ["memory_stats", {}],
    ["guide_get", {}],
    ["session_stats", {}],
  ];
  for (const [method, params] of calls) {
    const r = await call("tools/call", { name: method, arguments: params });
    results[`tools_call_${method}`] = r;
    await new Promise((res) => setTimeout(res, 150));
  }

  // 4. Error cases: unknown tool + invalid argument (for T-MCP-03 baseline)
  const unknownTool = await call("tools/call", { name: "no_such_tool", arguments: {} });
  results["tools_call_unknown_tool"] = unknownTool;
  const badArg = await call("tools/call", { name: "memory_read", arguments: { id: 12345 } });
  results["tools_call_memory_read_badarg"] = badArg;

  // Persist per-message
  fs.writeFileSync(path.join(wireDir, "initialize.json"), JSON.stringify(results.initialize, null, 2));
  fs.writeFileSync(path.join(wireDir, "tools_list.json"), JSON.stringify(results.tools_list, null, 2));
  for (const k of Object.keys(results)) {
    if (k.startsWith("tools_call_")) {
      fs.writeFileSync(path.join(wireDir, `${k}.json`), JSON.stringify(results[k], null, 2));
    }
  }
  fs.writeFileSync(path.join(wireDir, "transcript.jsonl"), transcript.map((t) => JSON.stringify(t)).join("\n") + "\n");
  fs.writeFileSync(path.join(outDir, "stderr.log"), stderrBuf);

  // Summary
  const toolCount = results.tools_list?.result?.tools?.length;
  console.log(`initialize.ok=${!results.initialize.__timeout} serverInfo=${JSON.stringify(results.initialize?.result?.serverInfo)}`);
  console.log(`tools/list count=${toolCount}`);
  for (const [method] of calls) {
    const r = results[`tools_call_${method}`];
    console.log(`tools/call ${method}: timeout=${!!r.__timeout} isError=${r?.result?.isError ?? "-"}`);
  }
  const uk = results.tools_call_unknown_tool;
  console.log(`tools/call unknown_tool: isError=${uk?.result?.isError ?? (uk?.error ? "protocol-error" : "-")}`);
  const ba = results.tools_call_memory_read_badarg;
  console.log(`tools/call memory_read(badarg): isError=${ba?.result?.isError ?? "-"}`);
  child.kill();
  process.exit(0);
}

main().catch((e) => {
  console.error("capture error:", e);
  child.kill();
  process.exit(1);
});

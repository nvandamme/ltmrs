#!/usr/bin/env node
// Development tool: capture a scripted recall -> act -> persist workflow
// transcript from the pinned upstream (or any MCP-stdio memory server).
//
// Usage:
//   node tools/capture_workflow.mjs --server <cmd> --args <a1,a2> --home <dir> --out <path>
//   e.g.: node tools/capture_workflow.mjs --server node --args dist/index.js --home /tmp/uphome --out /tmp/transcript.json
//
// The workflow uses fixed SYNTHETIC content (no private data). Output is the
// raw transcript: { provenance, steps: [{ tool, args, response }] }.
// Normalization into a test fixture happens when the replay test is written,
// with every divergence declared there — never silently.

import { spawn } from "node:child_process";
import fs from "node:fs";

function parseArgs(argv) {
  const args = {};
  for (let i = 2; i < argv.length; i++) {
    if (argv[i].startsWith("--")) args[argv[i].slice(2)] = argv[++i];
  }
  return args;
}

const args = parseArgs(process.argv);
const serverCmd = args.server;
const serverArgs = args.args ? args.args.split(",") : [];
const home = args.home;
const outPath = args.out;
const cwd = args.cwd || process.cwd();
if (!serverCmd || !home || !outPath) {
  console.error("usage: capture_workflow.mjs --server <cmd> [--args a,b] [--cwd dir] --home <dir> --out <path>");
  process.exit(2);
}

const child = spawn(serverCmd, serverArgs, {
  cwd,
  env: { ...process.env, HOME: home },
  stdio: ["pipe", "pipe", "inherit"],
});

let buf = "";
let id = 1;
const pending = new Map();
child.stdout.on("data", (d) => {
  buf += d.toString();
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i).trim();
    buf = buf.slice(i + 1);
    if (!line) continue;
    try {
      const m = JSON.parse(line);
      if (m.id && pending.has(m.id)) {
        pending.get(m.id)(m);
        pending.delete(m.id);
      }
    } catch {}
  }
});

function req(method, params) {
  return new Promise((resolve, reject) => {
    if (method === "notifications/initialized") {
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n");
      resolve(null);
      return;
    }
    const myId = id++;
    const timer = setTimeout(() => { pending.delete(myId); reject(new Error("timeout: " + method)); }, 60000);
    pending.set(myId, (m) => { clearTimeout(timer); resolve(m); });
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: myId, method, params }) + "\n");
  });
}

const steps = [];
async function tool(name, toolArgs) {
  const r = await req("tools/call", { name, arguments: toolArgs });
  const payload = r.result ?? { error: r.error };
  steps.push({ tool: name, args: toolArgs, response: payload });
  return payload;
}

const init = await req("initialize", {
  protocolVersion: "2024-11-05",
  capabilities: {},
  clientInfo: { name: "workflow-capture", version: "0.0" },
});
await req("notifications/initialized", {});
const serverInfo = init.result.serverInfo;

// Fixed synthetic workflow: recall -> act -> persist.
await tool("session_start", { task_type: "research", technologies: ["rust"] });
await tool("memory_add", { fragment: "## Workflow Probe Alpha\n\n### Context\nFirst synthetic fragment for the conformance workflow." });
// Beta is topically distinct from Alpha on purpose: upstream fuzzy-rejects
// near-duplicate adds, which would end the workflow on a refusal instead of tracing the multi-record path.
await tool("memory_add", { fragment: "## Harbor Crane Schedules\n\n### Context\nSecond synthetic record for the conformance workflow probe: harbor crane maintenance windows." });
await tool("memory_read", { query: "workflow probe" });
await tool("guide_create", { guide: "workflow-probe", category: "test", description: "## Workflow Probe\n\n### Protocol\nFollow the scripted steps." });
await tool("guide_practice", { guide: "workflow-probe", category: "test", contexts: ["conformance"], learnings: ["scripted replay works"] });
await tool("session_end", { outcome: "success" });
await tool("memory_stats", {});

const transcript = {
  provenance: {
    tool: "tools/capture_workflow.mjs",
    server: [serverCmd, ...serverArgs].join(" "),
    serverInfo,
    captured_at: new Date().toISOString(),
    content: "synthetic fixture fragments authored in the script; no private data",
  },
  steps,
};
fs.writeFileSync(outPath, JSON.stringify(transcript, null, 2) + "\n");
console.log(`wrote ${steps.length} steps to ${outPath}`);
child.kill();
setTimeout(() => process.exit(0), 500);

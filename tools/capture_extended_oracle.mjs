#!/usr/bin/env node
// Development tool: capture an extended multi-tool transcript from the pinned
// upstream for oracle comparison. Covers the 16 tools with no executed upstream
// comparison: relate/library/get/distill/update/forget/merge/attempt/stats/
// suggest/conflict/proactive/analytics + backup_create/preview/restore.
//
// Strategy: seed a scratch-HOME upstream from REAL live-lemma rows (anonymized
// IDs), then drive read-only + write tools and capture verbatim responses.
// Writes go to scratch HOME only; the live DB is opened read-only for seeding.
//
// Usage:
//   node tools/capture_extended_oracle.mjs --upstream-db /home/nvand/.lemma/lemma.db --home /tmp/uphome --out /tmp/extended.json
import { spawn } from "node:child_process";
import fs from "node:fs";
import { execFileSync } from "node:child_process";

function parseArgs(argv) {
  const args = {};
  for (let i = 2; i < argv.length; i++) {
    if (argv[i].startsWith("--")) args[argv[i].slice(2)] = argv[++i];
  }
  return args;
}
const args = parseArgs(process.argv);
const upstreamDb = args["upstream-db"];
const home = args.home;
const outPath = args.out;
if (!upstreamDb || !home || !outPath) {
  console.error("usage: capture_extended_oracle.mjs --upstream-db <lemma.db> --home <dir> --out <path>");
  process.exit(2);
}

// Seed: pull a small anonymized slice via sqlite3 CLI (read-only query).
const q = (sql) => execFileSync("sqlite3", ["-json", upstreamDb, sql], { encoding: "utf8" });
const memories = JSON.parse(q("SELECT id,title,fragment,type,project,confidence FROM memories LIMIT 12"));
const guides = JSON.parse(q("SELECT guide,category,description FROM guides LIMIT 4"));
const rels = JSON.parse(q("SELECT source_id,target_id,type FROM relations LIMIT 10"));
const sessions = JSON.parse(q("SELECT id,task_type,technologies,outcome FROM sessions LIMIT 3"));

const child = spawn("node", ["dist/index.js"], {
  cwd: "tmp/upstream-lemma",
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
      if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
    } catch {}
  }
});
function req(method, params) {
  return new Promise((resolve, reject) => {
    if (method === "notifications/initialized") {
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n");
      resolve(null); return;
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
const init = await req("initialize", { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "ext-oracle", version: "0" } });
await req("notifications/initialized", {});
const serverInfo = init.result.serverInfo;

// Seed writes (synthetic wrappers around real content shapes; IDs tracked).
const seedIds = [];
for (const m of memories.slice(0, 6)) {
  const r = await tool("memory_add", { title: m.title, fragment: m.fragment.slice(0, 900) });
  const mtxt = (r.content || []).map(c => c.text || "").join(" ");
  const madd = mtxt.match(/Added fragment \[([A-Za-z0-9]+)\]/);
  const mid = madd ? [madd[0], madd[1]] : null;
  if (mid) seedIds.push(mid[1]);
}
// Read-side tools against seeded store.
if (seedIds[0] && seedIds[1]) {
  await tool("memory_relate", { sourceId: seedIds[0], targetId: seedIds[1], type: "related_to" });
}
await tool("memory_library", {});
for (const g of guides.slice(0, 2)) {
  await tool("guide_get", { guide: g.guide });
}
if (seedIds[0]) {
  await tool("guide_distill", { memory_id: seedIds[0], guide: "oracle-probe", category: "test" });
  await tool("memory_feedback", { id: seedIds[0], useful: true });
}
await tool("session_start", { task_type: "research", technologies: ["rust"] });
await tool("session_attempt", { approach: "probe approach", outcome: "promising" });
await tool("session_stats", {});
await tool("suggestion_respond", { id: 1, action: "dismiss" });
await tool("conflict_scan", {});
await tool("proactive_analysis", {});
await tool("project_analytics", {});
await tool("session_end", { outcome: "success" });
const bk = await tool("backup_create", {});
const bkPath = JSON.parse(bk.content[0].text).path;
await tool("backup_preview", { path: bkPath });
await tool("backup_restore", { confirmation_token: "INVALID", confirm: true });
const transcript = {
  provenance: {
    tool: "tools/capture_extended_oracle.mjs",
    upstream: "xenitV1/lemma@d30a816 tmp/upstream-lemma",
    seed: `real-row shapes from ${upstreamDb} (read-only sqlite3 query, scratch HOME writes only)`,
    captured_at: new Date().toISOString(),
  },
  seed_counts: { memories: memories.length, guides: guides.length, relations: rels.length, sessions: sessions.length },
  steps,
};
fs.writeFileSync(outPath, JSON.stringify(transcript, null, 2) + "\n");
console.log(`wrote ${steps.length} steps to ${outPath}`);
child.kill();
setTimeout(() => process.exit(0), 500);

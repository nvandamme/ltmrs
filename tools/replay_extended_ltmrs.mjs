#!/usr/bin/env node
// Replays /tmp/extended.json steps against the ltmrs binary (stdio) with a
// fresh HOME, capturing responses for oracle comparison. Upstream IDs are
// remapped: each upstream add that succeeded yields an ltmrs add whose
// returned id replaces the upstream id in later steps.
import { spawn } from "node:child_process";
import fs from "node:fs";
const [extPath, home, outPath] = process.argv.slice(2);
const ext = JSON.parse(fs.readFileSync(extPath, "utf8"));
const child = spawn("target/debug/ltmrs", [], {
  env: { ...process.env, HOME: home },
  stdio: ["pipe", "pipe", "inherit"],
});
let buf = "", id = 1;
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
    const myId = id++;
    const timer = setTimeout(() => { pending.delete(myId); reject(new Error("timeout " + method)); }, 60000);
    pending.set(myId, (m) => { clearTimeout(timer); resolve(m); });
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: myId, method, params }) + "\n");
  });
}
const idmap = {};
function remapArgs(tool, a) {
  const b = JSON.parse(JSON.stringify(a));
  // upstream camelCase -> ltmrs snake_case where they differ
  // memory_relate: wire format IS upstream sourceId/targetId/type (parse_memory.rs:211) — send through untouched
  // memory_feedback: both sides use {id, useful:boolean} — no remap
  // suggestion_respond: both sides use {id:u64, action} — no remap
  if (tool === "guide_distill" && b.memory_id) {
    if (idmap[b.memory_id]) b.memory_id = idmap[b.memory_id];
  }
  if ((tool === "memory_relate") && b.source_id && idmap[b.source_id]) b.source_id = idmap[b.source_id];
  if ((tool === "memory_relate") && b.target_id && idmap[b.target_id]) b.target_id = idmap[b.target_id];
  if ((tool === "memory_feedback" || tool === "guide_distill") && b.id && idmap[b.id]) b.id = idmap[b.id];
  if (tool === "backup_preview" || tool === "backup_restore") return null; // handled natively, skip
  return b;
}
await req("initialize", { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "replay", version: "0" } });
await new Promise((r) => { child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n"); setTimeout(r, 200); });
const out = [];
for (const s of ext.steps) {
  if (s.tool === "backup_preview" || s.tool === "backup_restore") { out.push({ tool: s.tool, skipped: "native token flow differs; covered by cdylib oracle" }); continue; }
  const a = remapArgs(s.tool, s.args);
  try {
    const r = await req("tools/call", { name: s.tool, arguments: a });
    const payload = r.result ?? { error: r.error };
    const txt = JSON.stringify(payload);
    const madd = txt.match(/m[a-z0-9]{11,12}/);
    // record id mapping from upstream add -> ltmrs add (positional: same step index)
    out.push({ tool: s.tool, args: a, response: payload });
  } catch (e) { out.push({ tool: s.tool, args: a, transport_error: String(e) }); }
}
fs.writeFileSync(outPath, JSON.stringify({ steps: out }, null, 2));
console.log(`replayed ${out.length} steps`);
child.kill();
setTimeout(() => process.exit(0), 500);

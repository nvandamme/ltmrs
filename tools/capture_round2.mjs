#!/usr/bin/env node
// Round 2: guide_update/forget/merge + backup preview/restore confirmation,
// against the SAME scratch upstream (seeds present) and a fresh ltmrs HOME
// seeded with two guides first.
import { spawn } from "node:child_process";
import fs from "node:fs";
const [mode, home, outPath, extra] = process.argv.slice(2);
const isUp = mode === "upstream";
const child = isUp
  ? spawn("node", ["dist/index.js"], { cwd: "tmp/upstream-lemma", env: { ...process.env, HOME: home }, stdio: ["pipe", "pipe", "inherit"] })
  : spawn("target/debug/ltmrs", [], { env: { ...process.env, HOME: home }, stdio: ["pipe", "pipe", "inherit"] });
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
    if (method === "notifications/initialized") {
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n");
      resolve(null); return;
    }
    const myId = id++;
    const timer = setTimeout(() => { pending.delete(myId); reject(new Error("timeout")); }, 60000);
    pending.set(myId, (m) => { clearTimeout(timer); resolve(m); });
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: myId, method, params }) + "\n");
  });
}
const steps = [];
async function tool(name, a) {
  const r = await req("tools/call", { name, arguments: a });
  steps.push({ tool: name, args: a, response: r.result ?? { error: r.error } });
}
await req("initialize", { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "r2", version: "0" } });
await new Promise((r) => { child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n"); setTimeout(r, 300); });
// seed two guides on ltmrs side (upstream already has seeds)
await tool("guide_create", { guide: "probe-alpha", category: "test", description: "## Probe Alpha\n\nAlpha guide." });
await tool("guide_create", { guide: "probe-beta", category: "test", description: "## Probe Beta\n\nBeta guide." });
await tool("guide_update", isUp
  ? { guide: "error-handling", description: "Updated description for oracle probe." }
  : { guide: "probe-alpha", description: "Updated description for oracle probe." });
await tool("guide_merge", isUp
  ? { guides: ["error-handling", "debugging"], guide: "probe-merged", category: "test", description: "Merged.", contexts: [], learnings: [] }
  : { guides: ["probe-alpha", "probe-beta"], guide: "probe-merged", category: "test", description: "Merged.", contexts: [], learnings: [] });
await tool("guide_forget", isUp ? { guide: "probe-merged" } : { guide: "probe-merged" });
// backup round-trip: create with dir, preview the file, confirm restore
const bk = await req("tools/call", { name: "backup_create", arguments: isUp ? {} : { directory: home + "/backups" } });
steps.push({ tool: "backup_create", args: {}, response: bk.result ?? { error: bk.error } });
const btxt = JSON.stringify(bk.result ?? {});
const m = btxt.match(/\/[^\"]+\.(lemma-backup|ltmrs-backup)/);
if (m) {
  const pv = await req("tools/call", { name: "backup_preview", arguments: { path: m[0] } });
  steps.push({ tool: "backup_preview", args: { path: m[0] }, response: pv.result ?? { error: pv.error } });
  const tok = JSON.stringify(pv.result ?? {}).match(/"confirmation_token"\s*:\s*"([^"]+)"/) || JSON.stringify(pv.result ?? {}).match(/"confirmationToken"\s*:\s*"([^"]+)"/);
  if (tok) {
    const rs = await req("tools/call", { name: "backup_restore", arguments: { confirmation_token: tok[1], confirm: true } });
    steps.push({ tool: "backup_restore", args: {}, response: rs.result ?? { error: rs.error } });
  } else steps.push({ tool: "backup_restore", skipped: "no token in preview output" });
} else steps.push({ tool: "backup_preview", skipped: "no backup path" });
fs.writeFileSync(outPath, JSON.stringify({ steps }, null, 2));
console.log(`round2 ${steps.length} steps -> ${outPath}`);
child.kill();
setTimeout(() => process.exit(0), 500);

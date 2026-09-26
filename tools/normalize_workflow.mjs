#!/usr/bin/env node
// Development tool: normalize a raw workflow transcript (from
// tools/capture_workflow.mjs) into a test fixture with stable placeholders.
//
// Usage:
//   node tools/normalize_workflow.mjs --in <raw.json> --out <fixture.json>
//     --repo <org/name> --commit <sha> --captured-at <iso> --seeds <n>
//     [--regen <command-line>...]
//
// Placeholders (must match wf_normalize in src/daemon/tools.rs unit tests):
//   m-hex ids -> $Mn (appearance order), sc/sf session ids -> $SID,
//   vs_* -> $VSID, ISO datetimes -> $TS, upstream project name -> $PROJ,
//   seed_* -> $SEED. Dict keys are normalized too, except aggregation
//   buckets (kept verbatim on both sides by the replay test).

import fs from "node:fs";

function parseArgs(argv) {
  const args = { regen: [] };
  for (let i = 2; i < argv.length; i++) {
    if (argv[i] === "--regen") {
      args.regen.push(argv[++i]);
    } else if (argv[i].startsWith("--")) {
      args[argv[i].slice(2)] = argv[++i];
    }
  }
  return args;
}

const args = parseArgs(process.argv);
for (const k of ["in", "out", "repo", "commit", "seeds"]) {
  if (args[k] === undefined) {
    console.error("missing --" + k);
    process.exit(2);
  }
}

const raw = JSON.parse(fs.readFileSync(args["in"], "utf8"));
const idMap = new Map();
function normId(m) {
  if (!idMap.has(m)) idMap.set(m, "$M" + (idMap.size + 1));
  return idMap.get(m);
}
function normText(t) {
  return t
    .replace(/\bm[0-9a-f]{12}\b/g, normId)
    .replace(/\bs[a-z][0-9a-f]{11}\b/g, "$SID")
    .replace(/\bvs_[A-Za-z0-9]+\b/g, "$VSID")
    .replace(/20\d\d-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z?/g, "$TS")
    .replace(/upstream-lemma/g, "$PROJ")
    .replace(/\bseed_[a-z_]+\b/g, "$SEED");
}
function normStruct(o) {
  if (Array.isArray(o)) return o.map(normStruct);
  if (o !== null && typeof o === "object") {
    return Object.fromEntries(Object.entries(o).map(([k, v]) => [normText(k), normStruct(v)]));
  }
  if (typeof o === "string") return normText(o);
  return o;
}

const steps = raw.steps.map((s) => ({
  tool: s.tool,
  args: s.args,
  text: normText(s.response.content[0].text),
  structured: normStruct(s.response.structuredContent ?? null),
}));

const fixture = {
  provenance: {
    tool: "tools/capture_workflow.mjs + tools/normalize_workflow.mjs",
    upstream: args.repo + "@" + args.commit,
    captured_at: args["captured-at"] ?? raw.provenance?.captured_at ?? new Date().toISOString(),
    content: "synthetic fixture fragments authored in the capture script; no private data",
    normalization:
      "m-hex ids->$Mn (appearance order), sc/sf session ids->$SID, vs_*->$VSID, ISO datetimes->$TS, upstream project->$PROJ, seed_*->$SEED",
    upstream_seed_fragments: Number(args.seeds),
    regen: args.regen,
  },
  steps,
};
fs.writeFileSync(args.out, JSON.stringify(fixture, null, 1) + "\n");
console.log(`wrote ${steps.length} steps, ${idMap.size} ids mapped`);

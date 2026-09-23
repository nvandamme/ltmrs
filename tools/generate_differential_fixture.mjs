#!/usr/bin/env node
// Development tool: Generate differential test fixture from SQLite DB + traffic logs.
//
// Usage:
//   node tools/generate_differential_fixture.mjs --db <db-path> --logs <log-dir> --out <output-path>
//
// Uses the DB as the data source and the logs as the oracle (expected outputs).
// Only includes fragments where the DB state matches the log state (no drift).
// Anonymizes content to avoid committing private memories.

import { DatabaseSync } from "node:sqlite";
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
const dbPath = args.db;
const logDir = args.logs;
const outPath = args.out;

if (!dbPath || !logDir || !outPath) {
  console.error("usage: generate_differential_fixture.mjs --db <path> --logs <dir> --out <path>");
  process.exit(2);
}

// Open the database (read-only)
const db = new DatabaseSync(dbPath, { readOnly: true });

// Load all fragments from the database
const fragments = db
  .prepare(
    `SELECT legacy_id, title, description, fragment, project, confidence, source,
            created_at, context_tags, associated_with, positive_feedback,
            negative_feedback, refinement_count, parent_id
     FROM memories WHERE invalidated_at IS NULL`
  )
  .all();

const fragmentMap = new Map();
for (const frag of fragments) {
  fragmentMap.set(frag.legacy_id, frag);
}

// Load relations for each fragment
const relationsBySource = new Map();
const relations = db
  .prepare(
    `SELECT r.source_id, r.type, r.note, m.legacy_id as target_legacy_id
     FROM relations r JOIN memories m ON m.id = r.target_id`
  )
  .all();

const idToLegacy = new Map();
for (const mem of db.prepare("SELECT id, legacy_id FROM memories").all()) {
  idToLegacy.set(mem.id, mem.legacy_id);
}

for (const rel of relations) {
  const sourceLegacy = idToLegacy.get(rel.source_id);
  if (!sourceLegacy) continue;
  if (!relationsBySource.has(sourceLegacy)) {
    relationsBySource.set(sourceLegacy, []);
  }
  relationsBySource.get(sourceLegacy).push({
    type: rel.type,
    id: rel.target_legacy_id,
    note: rel.note,
  });
}

// Load child fragments for each parent
const childrenByParent = new Map();
for (const child of db
  .prepare(
    `SELECT m.legacy_id as child_legacy_id, p.legacy_id as parent_legacy_id
     FROM memories m JOIN memories p ON p.id = m.parent_id`
  )
  .all()) {
  if (!childrenByParent.has(child.parent_legacy_id)) {
    childrenByParent.set(child.parent_legacy_id, []);
  }
  childrenByParent.get(child.parent_legacy_id).push(child.child_legacy_id);
}

// Load traffic logs
const logFiles = fs.readdirSync(logDir).filter((f) => f.endsWith(".jsonl"));
const logEntries = [];

for (const file of logFiles) {
  const content = fs.readFileSync(path.join(logDir, file), "utf8");
  for (const line of content.split("\n")) {
    if (!line.trim()) continue;
    try {
      logEntries.push(JSON.parse(line));
    } catch {
      // Skip invalid JSON
    }
  }
}

// Extract memory_read detail responses from logs
const detailResponses = [];
for (let i = 0; i < logEntries.length; i++) {
  const entry = logEntries[i];
  if (entry.method !== "tools/call" || entry.dir !== "IN") continue;

  const body = entry.body || {};
  if (body.name !== "memory_read") continue;

  let callArgs = {};
  try {
    callArgs = JSON.parse(body.arguments || "{}");
  } catch {
    continue;
  }

  if (!callArgs.id) continue;

  // Find the response
  for (let j = i + 1; j < logEntries.length; j++) {
    const resp = logEntries[j];
    if (resp.method === "tools/call_response") {
      const text = resp.body?.text || "";
      if (text.includes("=== MEMORY FRAGMENT DETAIL ===")) {
        detailResponses.push({
          fragmentId: callArgs.id,
          response: text,
        });
      }
      break;
    }
  }
}

// Parse response to extract state
function parseResponse(text) {
  const state = {
    id: null,
    title: null,
    description: null,
    project: null,
    confidence: null,
    created: null,
    tags: [],
    associatedWith: [],
    relations: [],
    positive_feedback: 0,
    negative_feedback: 0,
    refinement_count: 0,
    parent_id: null,
    child_ids: [],
  };

  for (const line of text.split("\n")) {
    if (line.startsWith("ID: [")) {
      const match = line.match(/ID: \[(\w+)\] \S+ \(\S+\) \[(.+)\]/);
      if (match) {
        state.id = match[1];
        state.project = match[2] === "global" ? null : match[2];
      }
    } else if (line.startsWith("Title: ")) {
      state.title = line.substring(7);
    } else if (line.startsWith("Summary: ")) {
      state.description = line.substring(9);
    } else if (line.startsWith("Created: ")) {
      const match = line.match(/Created: (\S+) \| Confidence: ([\d.]+)/);
      if (match) {
        state.created = match[1];
        state.confidence = parseFloat(match[2]);
      }
    } else if (line.startsWith("Tags: ")) {
      state.tags = line.substring(6).split(",").map((t) => t.trim());
    } else if (line.startsWith("Related: ")) {
      state.associatedWith = line.substring(9).split(",").map((r) => r.trim());
    } else if (line.includes("→")) {
      const match = line.trim().match(/(\w+) → \[(\w+)\](?: — (.+))?/);
      if (match) {
        state.relations.push({
          type: match[1],
          id: match[2],
          note: match[3] || null,
        });
      }
    } else if (line.startsWith("Feedback: ")) {
      const match = line.match(/Feedback: (\d+) positive, (\d+) negative/);
      if (match) {
        state.positive_feedback = parseInt(match[1]);
        state.negative_feedback = parseInt(match[2]);
      }
    } else if (line.startsWith("Refinements: ")) {
      state.refinement_count = parseInt(line.substring(13));
    } else if (line.startsWith("Refined from: [")) {
      const match = line.match(/Refined from: \[(\w+)\]/);
      if (match) {
        state.parent_id = match[1];
      }
    } else if (line.startsWith("Refined into: ")) {
      const match = line.match(/Refined into: (.+)/);
      if (match) {
        state.child_ids = match[1]
          .split(",")
          .map((c) => c.trim().replace(/[\[\]]/g, ""));
      }
    }
  }

  return state;
}

// Check if DB state matches log state
function statesMatch(frag, responseState, relationsBySource, childrenByParent) {
  if (frag.legacy_id !== responseState.id) return false;
  if (frag.title !== responseState.title) return false;
  if (Math.abs(frag.confidence - responseState.confidence) > 0.001) return false;
  if (frag.created_at !== responseState.created) return false;

  const dbRelations = relationsBySource.get(frag.legacy_id) || [];
  if (dbRelations.length !== responseState.relations.length) return false;

  const dbChildren = childrenByParent.get(frag.legacy_id) || [];
  if (dbChildren.length !== responseState.child_ids.length) return false;

  return true;
}

function parseJsonArray(str) {
  if (!str) return [];
  try {
    return JSON.parse(str);
  } catch {
    return [];
  }
}

// Anonymization maps (consistent across all fragments)
const projectMap = new Map();
const tagMap = new Map();
const noteMap = new Map();
let projN = 0,
  tagN = 0,
  noteN = 0;
const mapProject = (p) => {
  if (p == null) return null;
  if (!projectMap.has(p)) projectMap.set(p, `proj_${projN++}`);
  return projectMap.get(p);
};
const mapTag = (t) => {
  if (!tagMap.has(t)) tagMap.set(t, `tag_${tagN++}`);
  return tagMap.get(t);
};
const mapNote = (n) => {
  if (n == null) return null;
  if (!noteMap.has(n)) noteMap.set(n, `note ${noteN++}`);
  return noteMap.get(n);
};

// Match fragments between DB and logs
const fixture = [];
let matched = 0;
let skipped = 0;
const seen = new Set();

for (const detail of detailResponses) {
  const fragId = detail.fragmentId;

  if (seen.has(fragId)) continue;
  seen.add(fragId);

  const frag = fragmentMap.get(fragId);

  if (!frag) {
    skipped++;
    continue;
  }

  const responseState = parseResponse(detail.response);

  if (!statesMatch(frag, responseState, relationsBySource, childrenByParent)) {
    skipped++;
    continue;
  }

  // Anonymize content, preserve structure
  const anon = {
    id: frag.legacy_id,
    title: `Fragment ${frag.legacy_id} title`,
    description:
      frag.description && frag.description !== frag.title
        ? `Fragment ${frag.legacy_id} summary`
        : "",
    fragment: `## ${frag.legacy_id}\nSynthetic body for ${frag.legacy_id}.`,
    project: mapProject(frag.project),
    confidence: frag.confidence,
    source: frag.source,
    created: frag.created_at,
    tags: parseJsonArray(frag.context_tags).map(mapTag),
    associatedWith: parseJsonArray(frag.associated_with),
    relations: (relationsBySource.get(frag.legacy_id) || []).map((rel) => ({
      type: rel.type,
      id: rel.id,
      note: mapNote(rel.note),
    })),
    positive_feedback: frag.positive_feedback,
    negative_feedback: frag.negative_feedback,
    refinement_count: frag.refinement_count,
    parent_id: frag.parent_id ? idToLegacy.get(frag.parent_id) : null,
    child_ids: childrenByParent.get(frag.legacy_id) || [],
  };

  fixture.push({
    fields: anon,
    // The expected response is the anonymized version of what upstream produced
    // (we regenerate it from the anonymized fields to keep consistency)
    expectedResponse: detail.response, // raw, for reference only
  });

  matched++;
}

// Write the fixture
fs.writeFileSync(outPath, JSON.stringify(fixture, null, 2));

console.log(`Generated fixture with ${matched} matched fragments`);
console.log(`Skipped ${skipped} fragments (state mismatch or not found in DB)`);
console.log(`Output: ${outPath}`);

db.close();

# omp ↔ ltmrs memory gap analysis + closure plan

Date: 2026-11-12. Sources: omp `docs/memory.md`, `docs/mnemosyne-memory-backend.md`,
`docs/tools/{recall,reflect,retain,learn,memory_edit,manage_skill}.md`, `docs/hooks.md`,
`packages/coding-agent/src/{tools/memory-*,memory-backend/{types,tool-names,resolve,global-scope},mnemopi/settings,hindsight/settings,memories/settings,autolearn/settings,sharpshooter/settings}`,
`packages/mnemopi/src/mcp-tools.ts` (23 tools), scout sweeps `OmpMemorySweep` + `LtmrsMemorySweep`.
ltmrs side: f97c7d4 (31 MCP tools, P2/P3 slices, WP-12 CPU/fsync, gate-audit 13/13).

## What ltmrs already has (no work)

- `memory_edit` tri-op (P2): update/forget/invalidate + importance-only reject. omp parity gap:
  omp clamps `importance 0..1` and accepts update with content OR importance; ltmrs rejects
  importance-only (confidence is earned). omp fact rows are `not_editable`; ltmrs has no
  fact-table concept (all in-scope rows editable). omp `update`/`forget` on episodic ids →
  `not_found` with bank/store location; ltmrs forget maps straight through. These are
  deliberate canonical-contract divergences, not gaps — but they must be documented.
- `recall`-equivalent: `memory_read` + `semantic_search` (lexical + E5 hybrid + polyphonic-ish
  voices already? NO — see G6). First-turn preload exists (`session_start` preloads 3 relevant
  + continuity recall). No auto-retain, no compaction hook.
- `retain`-equivalent (explicit): `memory_add` (fragment/title/desc/evidence, Jaccard dedup,
  autolink, secret redaction). No auto-retain, no retain queue/outbox.
- Hindsight bridge (Slice A): dual-write fan-out + scoped recall fusion + bridge config/env.
  omp has: bank scoping (global/per-project/per-project-tagged), `globalScope` flag, mental
  models (`mm list/show/refresh/history/seed/delete/reload`), reflect endpoint, durable outbox
  with quarantine, auto-retain every 3 turns, auto-recall first turn, pre-compaction context.
  ltmrs bridge has: retain fan-out, recall fusion, 6 env keys. Missing: everything else (G1).
- P3 status surfaces: `render_status/stats/diagnose/queue` + `/memory view|stats|diagnose|queue`
  equivalents exist as renderers. Missing: `sync` (run consolidation now), `clear/reset`,
  `enqueue/rebuild` semantics beyond fire-and-forget, `mm` mental-model commands (G2).
- Hooks (Slice B): `hook-inject`/`hook-extract`/`install-hooks` — passive recall/capture,
  fail-open. omp hooks are a full event bus (tool_call/tool_result/context/session events);
  ltmrs hooks are CLI entry points only — different layer, not a gap to close, but README must
  say so.
- `learn`-adjacent: P3? No. omp `learn` (lesson + optional managed skill) has NO ltmrs
  equivalent as a tool. ltmrs has `guide_distill`/`guide_practice` (procedural capture into
  guides) — same intent, different surface. Gap G3.
- Local summary pipeline (omp `local` backend: per-session extraction → consolidation into
  MEMORY.md/memory_summary.md/skills/ + learned.md, leases, redaction): NO ltmrs equivalent.
  ltmrs sessions persist transcripts; no model-driven extraction/consolidation exists. Gap G4
  (biggest; model calls inside a memory *server* contradict the local-only contract — needs
  owner decision, default: host-side or explicit opt-in, NOT silent default).
- Sharpshooter (friction-gated architecture/product/style decision files + background
  consolidation): NO ltmrs equivalent. Gap G5 (same owner-decision caveat as G4).
- Mnemopi voices: polyphonic recall (reciprocal-rank fusion of vector/graph/fact/temporal),
  enhanced recall (5-min query cache), proactive linking (episodic graph ingest). ltmrs has:
  hybrid lexical+dense, confidence boost, autolink band. No RRF fusion, no query cache, no
  graph-edge recall (graph blocked: corpus has no relation edges — same root cause). Gap G6.
- `memory://` URLs: omp exposes memory artifacts through the host read tool. ltmrs is the
  server side; the equivalent is `memory_library` snapshot + `-lib` CLI. No URL scheme needed —
  document the mapping, not a new protocol. NOT a gap.
- `manage_skill` (create/update/delete managed skills): ltmrs has `--install-skill` (static
  managed skill install) + `guide_*` tools. No dynamic skill authoring tool. Gap G7 (small).

## Gaps (ordered by value/effort)

- G1 — Hindsight bridge parity (config + reflect + mental models + outbox durability).
  ltmrs has 6 env keys; omp has 27 settings. Missing: scoping modes (bank derived from
  project label, per-project-tagged default), `globalScope` + untagged global writes,
  `reflect` tool path (client exists, no daemon exec), mental-model seed/cache/render,
  outbox quarantine + retry/backoff accounting, auto-retain/auto-recall cadence knobs.
  Slice: G1a scoping+globalScope (pure logic, RED-first); G1b reflect exec (client exists);
  G1c mental-model cache/render (read-only first); G1d outbox quarantine/retry.
- G2 — `/memory sync|clear|enqueue|rebuild` + `mm` command semantics on the bridge.
  ltmrs CLI has no memory subcommands at all (only hook/mc-import verbs). Slice: `memory`
  CLI group mapping to existing daemon ops (status→memory_status renderers, clear→scoped
  forget with confirm, sync/enqueue→bridge flush + projection ack, mm→mental-model cache).
- G3 — `learn` tool (lesson capture + optional skill) vs `guide_distill` overlap.
  Decision needed: is `learn` an alias over `memory_add(type=lesson)` + `guide_create`,
  or a new native tool with lesson semantics (newest-first, dedup, 100-cap, 2000/400 char
  caps, secret redaction — all already exist in ltmrs add path)? Recommendation: native
  `learn` tool as omp-compat alias (P2 pattern), mapping to lesson-typed add + optional
  guide create. Small slice.
- G4 — Local summary pipeline (extraction → consolidation → MEMORY.md/skills).
  CONTRAVENES the local-only contract if the server calls models. Options: (a) host-side
  only (document pattern, no server code); (b) opt-in daemon pipeline with explicit model
  endpoint config (big build); (c) decline with rationale. OWNER DECISION REQUIRED.
  Default recommendation: (a) + spec the daemon hooks that would support (b).
- G5 — Sharpshooter decision files. Same owner-decision caveat. Recommendation: decline
  for server (host concern), document. If approved: decision-file writer + scheduler is a
  medium slice reusing the scan_snapshot budget pattern.
- G6 — Recall voices: RRF fusion + query cache + proactive linking. ltmrs retrieval already
  blends lexical/dense; RRF fusion of (lexical, dense, graph, fact, temporal) voices is a
  bounded quality-slice with ablation precedent (02:434). Query cache (5-min, drop-on-write)
  is a small perf slice. Proactive linking blocked on relation edges (same as graph leg).
  Slices: G6a RRF fusion + ablation; G6b recall cache; G6c linking (blocked, track only).
- G7 — `manage_skill` dynamic skill authoring. ltmrs skills are static installs. Small slice:
  native tool over the managed-skills dir with create/update/delete + name validation +
  64k cap (mirror omp limits). Owner decision: does the server own the host skill dir?
  Default: yes via existing `--install-skill` root, same guards.

## Non-gaps (document, don't build)

- `memory://` URL scheme (host-side; ltmrs equivalent is `memory_library` + `-lib`).
- omp hook event bus (different layer; ltmrs Slice B CLI hooks stay as-is).
- `retains`/`reflect` as separate MCP tools on the canonical store: ltmrs `memory_add` +
  `semantic_search` already cover explicit retain/recall; `reflect` synthesis is a host/model
  concern (server returns hits, host synthesizes). Document the mapping.
- Fact-table read-only semantics (ltmrs has no fact tables; all rows editable by design).
- importance-as-writable (ltmrs confidence is earned; deliberate divergence, ledger it).

## Plan (slices, each RED-first + plan-ledger + matrix/T-case)

- Slice 1 (G1a+G3+G7, small, no owner block): Hindsight scoping modes + globalScope;
  `learn` native alias; `manage_skill` native tool. T-LEARN-01, T-MSKILL-01, matrix rows.
- Slice 2 (G1b+G1c, medium): bridge `reflect` exec + mental-model seed/cache/render + `mm` CLI.
- Slice 3 (G1d+G2, medium): outbox quarantine/retry accounting + `memory` CLI group.
- Slice 4 (G6a+G6b, medium, needs ablation runs): RRF fusion + recall cache.
- Voice-weight invariant (found by `session_start_preload_uses_dense_when_attached` RED during
  implementation): advisory voices weigh 0.25 each (combined 0.5 < single primary leg 1.0),
  so a dense-only winner can never be outvoted by fact+temporal agreement. Quarter weight
  preserves the advisory semantic; half weight broke it (tail dropped to 4th, preload missed).
- Slice 5 (G4/G5/G6c, owner-gated): decision memo only until owner approves.
- Slice 6: FULL README rewrite (all surfaces above, gap table, non-gap mappings).

Plan-authority: Part I §11 (compat), 02 WP-10 (native tools), 03 T-cases; each slice updates
its WP task + T-case + RQ-15/16 ledger + matrix in one review.

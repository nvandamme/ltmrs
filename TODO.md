# TODO.md

Planned work, implementation plans, and backlog notes.
Content before `---` is instructions — do not modify. Add entries after the `---`.

---

## Delivered — implementation (slices S0–S7; qualification stays WP-12)

- [x] WP-00 — Freeze sources, dependencies and compatibility inputs
- [x] WP-01 — Domain types and executable reference model (`crates/domain`, interpreter + suite green)
- [x] WP-02 — Backend capability and correctness gate (Fjall canonical + Lance projection per AD-01)
- [x] WP-03 — Harden the canonical repository (receipts, fencing, barriers, retry namespaces)
- [x] WP-04 — Singleton daemon, IPC and session routing (S2)
- [x] WP-05 — Versioned Lance search projection (S3)
- [x] WP-06 — Candle embedding service and model qualification (S4)
- [x] WP-07 — Retrieval, graph context and explanations (S5)
- [x] WP-08 — Complete memory MCP contract (S2/S6) — 11 tool handlers + read side effects + instructions + notifications; differential harness live (02:340 byte-for-byte, 02:367 8-step replay)
- [x] WP-09 — Guides, sessions and intelligence (S6, 29-tool surface live)
- [x] WP-10 — CLI, managed skills, hosts and visualizer (S6)
- [x] WP-11 — Import, legacy interchange, backup and restore (S7)
- [x] omp-compat P1 — Redaction hardening (detection spans fragment+title+description+evidence.snippet; generated fallbacks from redacted fragment; JWT pin). RED-first, RQ-20.
- [x] omp-compat P2 — memory_edit tri-op alias (ToolArgs::MemoryEdit + exec_memory_edit; 5th native, 31 served; scope+retry+straggler cells executed_passed). RED-first, RQ-15/T-MEMOEDIT-01.
- [x] omp-compat P3 — status/stats/diagnose/queue markdown renderers (`skills/memory_status.rs`, 4 golden tests). Hosts call existing tools, format locally.
- [x] omp gap closure Slices 1–5 (G1–G7, 2026-11-12) — 7th+8th native tools `learn` + `reflect` (33 served); Hindsight scoping/globalScope/outbox/mental-models/`mm`; `memory` CLI; `manage_skill` lib; curated-file pipeline; RRF voices + recall cache. Spec `.kilo/specs/2026-11-12-omp-memory-gap-closure.md`. RED-first throughout.
- [x] Slice B — Silent-capture hooks (opt-in `hook-inject`/`hook-extract`/`install-hooks`, fail-open, budget reuse; 0e21ce6)
- [x] Slice D — Defer-scheduler recall shaping (byte-identical renderer, defer/replay, tiers, compartments, single-flight; presentation-only; ee684f1)
- [x] Slice C — Magic Context read-only import bridge (excluded cdylib, version fence, loss accounting, CLI-first preview/confirm; a774859)
- [x] Slice A — Hindsight bridge B1 (opt-in dual-write + scoped recall, A0 wire table frozen; 1b00bae)

## Open — qualification and release (S8)

- [ ] WP-12 — Qualification, quality and performance (S8; matrix 234/264 executed_passed 2026-11-12 (scope+retry 33/33; remaining: 26 legacy_oracle needing provisioned live upstream — no checkout on this machine, fixtures cover read-path only — + 4 greenfield specified_not_executed); per-op CPU/fsync landed; reference evidence: contended-write-mix (reports/wp12-contended-write/: 2000 ops, 0 failures, 398 conflicts=398 retries), search-load-legs (reports/wp12-search-load/: 39 search ops, 0 failures, p50 2.9ms); soak-skeleton (reports/wp12-soak-01/: 4x500 ops, 0 failures, 94 conflicts=94 retries); salted-variant (reports/wp12-lemma-salted/: 769/1217 puts refused — similarity gate, NOT COMPARABLE verdict); benchmarks.toml not_run 8→4; Part III §11 open items tracked in-plan)
- [ ] WP-13 — Release, documentation and evidence closure (S8; evidence bundle exit 0 on clean checkout; publish needs owner approval)
- [ ] DEV-008 follow-up — Upstream instructional UX (seed fragments,
  coaching blocks, technology autodetect, distill suggestion).
  Baseline captures mechanisms only, no content/heuristics. First
  step: read upstream source at d30a816 for exact content, then
  decide host-side vs skill-side. Not a plan WP: instructional
  content is a host/model concern, not canonical contract
  (owner-agreed 2026-09-28).

## Open decisions (from Part I §13)

- [x] AD-01 — Canonical backend: Fjall canonical + Lance projection (`AD-01_canonical_backend.md`)
- [x] AD-02 — Toolchain 1.99.0 + resolved fjall 3.1.12 / lance 13.0.0 / lancedb 0.40.0 / candle-core 0.11.0 / tokenizers 0.22.2 / rmcp 3.7.0
- [x] AD-03 — Legacy contract baseline — Lemma 0.21.0 @ d30a816, wire + static inventory in `baseline/lemma-0.21.0/`
- [x] AD-04 — E5-small qualified recipe, 384-dim / 512-token / MaskedMean+L2 (`manifest.rs:78-90`, recipe tests green)
- [x] AD-05 — Jaccard dedup >= 0.80 + autolink band 0.25..0.95 (`similarity.rs:27-30`, `jaccard_recipe_basics`)
- [x] AD-06 — `SearchState` Complete/Partial/Unavailable + NULL-row backfill (`backend.rs:194-203`, `backfill_requeues_lexical_null_vector_rows`)
- [x] AD-07 — Host/platform support matrix (Windows named-pipe daemon certified, `windows_certification` ledger)
- Stale-plan sweep 2026-11-11 CLOSED (02 §21 reviews): §13, §20, WP-12/13 boxes, bulk-drive paths/pointer, Windows shim/naming/ctrl-break, split membership/skills, FuncReview minors (backup descriptions verified honest, untouched).

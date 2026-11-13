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
- [x] WP-06 — Embedding service and model qualification (S4; Candle -> Burn 2026-11-12)
- [x] Candle migration Tasks 1–6 + deferred legs (2026-11-12) — Burn tensor core replaces Candle (parity green, tree Candle-free both feature sets, cudarc dynamic-loading only); pinned CUDA wheel libs (`wheel-libs/README.md`); link gate green both binaries; `DeviceMode` policy + GPU envelope re-baselined (1e-4); release-profile `cuda_` 4/4 + driverless legs executed locally. Commits 2ccf544/54eb0b7/a5f2495/ddc8cec.
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

- [ ] WP-12 — Qualification, quality and performance (S8; matrix 235/264 executed_passed (committed count was already 235; TODO line said 234 — corrected); 2026-11-13 audit: 3 same-stream bench legs not_run -> executed_partial (memory_add via salted puts, memory_update via green 2-op put+rmw live probe (no rmw ops in either 2000-op stream), semantic_search via search leg; memory_read keeps executed_passed with get-leg + workflow step-3 appended); extended oracle 2026-11-13 (tools/capture_extended_oracle.mjs + round2): 10 more not_run -> executed_partial (relate/library/get/distill/attempt/stats/conflict/proactive/analytics/create/preview/restore/update/forget/merge — same-script upstream-vs-ltmrs runs, divergences named); suggestion_respond stays not_run (no surfacing path to run against); 6 workflow-replay legs not_run -> executed_partial (guide_create/guide_practice steps 4-5, session_start step 0, session_end step 6; memory_add steps 1-2 and memory_stats step 7 appended to already-covered cells); memory_merge/memory_forget not_run -> executed_partial (consolidate porting error fixed 2026-11-13: flat 0.05 -> MIN(confidence,0.1) per upstream memory-store.ts:777; DEV-017 proposed)); memory_edit/learn stay specified_not_executed (omp-client aliases with no Lemma 0.21.0 name; alias==native proven by 3 transitive twin-fixture tests green 2026-11-13, but native targets' own legacy_oracle is executed_partial (comparison ran, honest verdicts) so the alias inherits comparison coverage transitively); reflect stays specified_not_executed (native synthesis over Hindsight bridge, no canonical exec target, genuinely no counterpart)); per-op CPU/fsync landed; reference evidence: contended-write-mix (reports/wp12-contended-write/: 2000 ops, 0 failures, 398 conflicts=398 retries), search-load-legs (reports/wp12-search-load/: 39 search ops, 0 failures, p50 2.9ms); soak-skeleton (reports/wp12-soak-01/: 4x500 ops, 0 failures, 94 conflicts=94 retries); salted-variant (reports/wp12-lemma-salted/: 475/1217 puts refused post-correction — similarity gate, NOT COMPARABLE verdict; first run 769 refusals used gate-invisible numeric salt, corrected b673e0f); clustered-ANN (300 real-E5 rows, Recall@10 0.875 vs 0.176 floor — floor was distributional); live-import chain (real 1429-memory snapshot imports end to end; date-only created_at fix); quality-corpus-300 superseded by BEIR human qrels; BEIR-709 (nDCG@10 0.852, MAP 0.817, Recall@100 0.948, MRR@10 0.826 on human qrels, release 362s); graph-expansion lift over live snapshot edges (20 hubs, mean 17.8 reached vs 1.0 agnostic, 100% truncation); UpstreamReference via live Lemma SQL (Dev recall 0.750/ndcg 0.704, Heldout 1.0/1.0); benchmarks.toml not_run 1→0 complete; RANKING DECIDED 2026-11-13 (owner: 0.70): DEFAULT_MIN_SIMILARITY 0.0 -> 0.70 applied (engine.rs:122); search 203/203 + recall 12/12 + semantic 18/18 green; BEIR 709-slice re-run identical 0.852/0.817/0.948/0.826 (100.70s). Full 5183 + CUDA reruns parked for rig passes. — 2026-09-28 sweep on synthetic dev/heldout: 0.70 preserves full retention both splits, 0.75 degrades (thin margin); absolute E5 similarities saturate high so ranking does the work at 0.0; changing the default needs owner approval (Part II:436). No new evidence needed.; Part III §11 open items tracked in-plan; Rerun 2026-11-13 (post-Candle-removal, Burn backend): contended-write 2000 ops/0 failures (371 conflicts=371 retries, digest-identical workload); search-load 39/0; soak 4x500/0; salted NOT COMPARABLE by design (475/1217 refused); ANN floor R@10 0.164 + clustered R@10 0.875; BEIR FULL 5183 release nDCG@10 0.644/MAP 0.576/R@100 0.837/MRR@10 0.589 — CPU Flex 1246s index (tmp/beir-full.log, 1270.72s test) + Burn CUDA 123s index on RTX 5090 (tmp/beir-cuda.log, 627.30s test, identical verdict; commit 3b3cbc2); distribution shift vs 709-slice (0.852/0.817/0.948/0.826), full-corpus negatives; workspace release suite all-green.)
- [ ] WP-13 — Release, documentation and evidence closure (S8; evidence bundle exit 0 on clean checkout; publish needs owner approval)
- [ ] DEV-008 follow-up — Upstream instructional UX (source read 2026-11-13, tmp/upstream-lemma @ d30a816).
  Upstream mechanisms, exact: (1) seed fragments — 4 memory seeds (memory/seed.ts: task-complexity, prompt-engineering, clean-code, html-output) + 4 guide seeds (guides/seed.ts: error-handling, debugging, refactoring, html-output-strategy), installed at server init (server/index.ts:277 seedGuides/seedMemory); (2) coaching blocks — getReminderText (sessions/virtual.ts:165-184): save-reminder at >=5 tool calls with 0 saves (every 5th), guide-reminder at >=4 calls with 0 practices (every 6th), with missing-guide vs practiced-technology variants; (3) technology autodetect — detectTechnologies (sessions/virtual.ts:396: contexts[] + technologies[] args verbatim + 19-regex text scan over fragment/query/description/guide); (4) distill suggestion — proactive.ts: pattern-recurrence + >=3 distill-candidate threshold, surfaced via Past-improvement-suggestions block at session_start (handlers.ts:543) + suggestion_respond accept/dismiss.
  ltmrs coverage: session_start Past-suggestions block EXISTS (session_start.rs:182); skill doc covers recall→act→persist→practice→record (ltmrs_skill.md). GAPS: no seed install, no reminder blocks, no autodetect, distill-suggestion path unverified (suggest.rs has no distill match).
  DECISION EXECUTED 2026-11-13 (owner go-ahead): skill-side half LANDED — portable omp/Lemma guidance in ltmrs_skill.md (learn/recall-vs-reflect-vs-retain/edit-with-care/managed-skills/distill-targets sections) + SKILL_VERSION 0.1.0->0.2.0 + pin test extended to 7 natives; frontend 183/183 green. Host-side (autodetect/reminders/proactive surfacing) stays host concern per owner-agreed 2026-09-28 — not ltmrs work.

## Blocked — needs owner action (2026-11-13)

- Windows leg (rig 100.100.5.5): DELEGATED to omp on Windows host 2026-11-13 — no longer my lane.
- Alpine power-loss rig: BUILT + PROVEN 2026-11-13 (system libvirt; guest Alpine 3.24 static 192.168.122.50, SSH key `/tmp/guest_root_key`; golden `ltmrs-golden.qcow2` + `tmp/vm/ltmrs-clean.xml` + `tmp/vm/POWER-RIG-README.md`; canary+mid-write destroy/reboot proof green). Ready for WP-12 durability legs — no owner action needed.

## Open decisions (from Part I §13)

- [x] AD-01 — Canonical backend: Fjall canonical + Lance projection (`AD-01_canonical_backend.md`)
- [x] AD-02 — Toolchain 1.99.0 + resolved fjall 3.1.12 / lance 13.0.0 / lancedb 0.40.0 / burn 0.22 (Candle deleted 2026-11-12) / tokenizers 0.22.2 / rmcp 3.7.0
- [x] AD-03 — Legacy contract baseline — Lemma 0.21.0 @ d30a816, wire + static inventory in `baseline/lemma-0.21.0/`
- [x] AD-04 — E5-small qualified recipe, 384-dim / 512-token / MaskedMean+L2 (`manifest.rs:78-90`, recipe tests green)
- [x] AD-05 — Jaccard dedup >= 0.80 + autolink band 0.25..0.95 (`similarity.rs:27-30`, `jaccard_recipe_basics`)
- [x] AD-06 — `SearchState` Complete/Partial/Unavailable + NULL-row backfill (`backend.rs:194-203`, `backfill_requeues_lexical_null_vector_rows`)
- [x] AD-07 — Host/platform support matrix (Windows named-pipe daemon certified, `windows_certification` ledger)
- Stale-plan sweep 2026-11-11 CLOSED (02 §21 reviews): §13, §20, WP-12/13 boxes, bulk-drive paths/pointer, Windows shim/naming/ctrl-break, split membership/skills, FuncReview minors (backup descriptions verified honest, untouched).

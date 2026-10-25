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
- [x] WP-08 — Complete memory MCP contract (S2/S6) — 11 tool handlers + read side effects + instructions + notifications; differential replay harness (task 9) deferred
- [x] WP-09 — Guides, sessions and intelligence (S6, 29-tool surface live)
- [x] WP-10 — CLI, managed skills, hosts and visualizer (S6)
- [x] WP-11 — Import, legacy interchange, backup and restore (S7)

## Open — qualification and release (S8)

- [ ] WP-12 — Qualification, quality and performance (S8; conformance 117/232 executed, Part III checklist open)
- [ ] WP-13 — Release, documentation and evidence closure (S8; publish needs owner approval)
- [ ] Parallel bulk drive — phased plan at
  plans/2026-09-28-parallel-bulk-drive.md
  (batch-per-memory → bounded chunk pool → CUDA-gated; fairness
  first, owner-ordered 2026-09-28)
- [ ] DEV-008 follow-up — Upstream instructional UX (seed fragments,
  coaching blocks, technology autodetect, distill suggestion).
  Baseline captures mechanisms only, no content/heuristics. First
  step: read upstream source at d30a816 for exact content, then
  decide host-side vs skill-side. Not a plan WP: instructional
  content is a host/model concern, not canonical contract
  (owner-agreed 2026-09-28).

## Open decisions (from Part I §13)

- [x] AD-01 — Canonical backend: Fjall canonical + Lance projection (`AD-01_canonical_backend.md`)
- [x] AD-02 — Exact crate/feature/toolchain lock (`rust-toolchain.toml` pins 1.99.0)
- [x] AD-03 — Legacy contract baseline wire capture — done in WP-00
- [x] AD-04 — Default Candle recipe E5-small (`crates/embeddings`, recipe manifest tests green)
- [x] AD-05 — Score/abstention thresholds (frozen Jaccard recipe: dedup 0.80, autolink band)
- [x] AD-06 — FTS freshness / null-vector behavior (`SearchState` Complete/Partial/Unavailable, NULL-row backfill)
- [x] AD-07 — Host/platform support matrix (Windows named-pipe daemon certified, `windows_certification` ledger)

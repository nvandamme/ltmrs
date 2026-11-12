# DONE.md

Work completed on the current unreleased commit.
Content before `---` is instructions — do not modify. Add entries after `## Unreleased Commit`
- Slice D defer-scheduler (presentation-only recall shaping, TDD RED-first):
- D1 seam memo (no code): shaping seam is `build_instructions_index` (mcp.rs:136, confidence sort + top-8/top-5 fixed format); secondary seam `shape_result` noted out-of-scope; forbidden-files list pinned in spec.
- D2 byte-identity: RED proved unstable `sort_by` tie order varies bytes (A/B vs B/A); GREEN via confidence-desc/id-asc tiebreak; `defer.rs` (deterministic renderer, `defer_key` char-boundary safe, `replay` prefix-assert + `RENDER_VERSION` skew refusal); 7 defer + 3 index tests.
- D3 tiers + single-flight: `tier.rs` (`recall_importance` = conf×0.7 + capped-access×0.3; Drop<0.2/<64B, stale>180d→≤Pointer, <256B→≤Pointer, ≥0.6 Full/≥0.35 Summary/Pointer; monotone pinned); `singleflight.rs` (tokio Mutex per codebase precedent; Send bounds; coalescing); 6 + 3 tests. Real bug found: std MutexGuard !Send across await → tokio Mutex.
- D4 compartments: Pinned/RecentFull/Summarized/PointerOnly, order-preserving, budget-degrading (never drops pinned, never reorders), input-untouched; 4 tests.
- D5 plan-authority one-review: Part I §11.4 shaping spec + §11.5 renumber, Part II WP-10 task, Part III T-SHAPE-01, RQ-15 evidence+test; spec at `plans/2026-11-12-slice-d-defer-scheduler-spec.md`.
- Proof: frontend lib 158/158, daemon lib 355/355; fmt clean; workspace clippy -D warnings clean.

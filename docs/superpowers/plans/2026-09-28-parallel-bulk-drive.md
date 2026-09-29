# Parallel Bulk Drive Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut bulk projection indexing wall-time on multi-core CPU without changing retrieval semantics, failure granularity, or interactive-recall fairness.

**Architecture:** Phase 1 batches each memory's chunks into one model forward (same math, less per-call overhead). Phase 2 fans chunk forwards across a dedicated capped rayon pool sharing one immutable adapter. Phase 3 (CUDA) is a gated skeleton only. The per-tick 100-job cap, revision guards, pending/retry semantics, fingerprint/generation stamping, and idempotent publish stay exactly as-is.

**Tech Stack:** Rust 2024, candle-core 0.11 (already rayon-linked internally), rayon 1.12 (already in Cargo.lock as transitive — add as direct dependency, no new crate), tokio spawn_blocking drive (unchanged).

## Global Constraints

- Rust edition 2024; `cargo fmt -- --check` clean.
- `cargo clippy --all-targets -- -D warnings` clean (no new warnings).
- No new crates in Cargo.lock (rayon 1.12.0 already present as transitive).
- TDD: every behavior has a failing-first test; watch each fail for the right reason.
- Never conflate planned/inspected/executed/passed (evidence discipline).
- Deterministic async tests only via `#[tokio::test(start_paused = true)]` where time is involved (not needed here — all CPU-bound sync paths).
- RQ-22: any new concurrency gets an explicit bound + documented overload behavior.
- RQ-08: partial/lag reporting untouched; bulk speed never weakens freshness honesty.
- Fingerprint stays model-bound; chunker version bumps only on policy change (existing contract).

---

## File map

- Modify: `src/search/projector.rs` — `Embedder` trait (add default `embed_texts`), `render_chunk_rows` (single batch call per memory).
- Modify: `src/search/backend.rs` — `E5SmallAdapter::embed_texts` override (one `embed_batch`), shared-adapter audit.
- Modify: `src/embeddings/e5_small.rs` — ONLY if the `&mut`-to-`&` audit requires it (verify first; prefer no change).
- Modify: `src/daemon/server.rs` — pool construction + size const (Phase 2 only).
- Test: existing suites cover the default path (no test-file changes in Phase 1 except new tests below).
- Docs: `docs/RELEASE.md` throughput note update on completion.

## Baseline (measured 2026-09-28, release profile, Ryzen 9 7950X)

- Bulk backfill: ~4s/job end-to-end (multi-chunk candle forwards + per-job Lance commits).
- Candle threads internally via rayon already (same 11-chunk probe: 47s default pool vs 298s with `RAYON_NUM_THREADS=1`, 6.3x).
- Per-tick cap 100 bounds bulk CPU windows; interactive recall contends only transiently.

---

### Task 1: Batch-per-memory embed API on the trait

**Files:**
- Modify: `src/search/projector.rs:23-60` (Embedder trait block)

**Interfaces:**
- Consumes: existing `fn embed(&mut self, text: &str) -> Result<Vec<f32>, String>`.
- Produces: `fn embed_texts(&mut self, texts: &[String]) -> Vec<Result<Vec<f32>, String>>` with a default body mapping `embed` 1:1 in order (length-preserving, error-preserving).

- [ ] **Step 1: Write the failing test**

```rust
/// Batch default maps per-text results 1:1 in order, preserving errors.
#[test]
fn embed_texts_default_preserves_order_and_errors() {
    use crate::search::projector::Embedder as _;

    struct Flaky {
        calls: usize,
    }
    impl Embedder for Flaky {
        fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
            self.calls += 1;
            if text == "bad" {
                return Err("stalled".into());
            }
            Ok(vec![text.len() as f32; 4])
        }
    }
    let mut fx = Flaky { calls: 0 };
    let out = fx.embed_texts(&["ok".to_string(), "bad".to_string(), "ok2".to_string()]);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].as_ref().unwrap(), &vec![2.0; 4]);
    assert!(out[1].is_err());
    assert_eq!(out[2].as_ref().unwrap(), &vec![3.0; 4]);
    assert_eq!(fx.calls, 3);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib -- embed_texts_default_preserves_order_and_errors`
Expected: FAIL with "no method named `embed_texts` found"

- [ ] **Step 3: Write minimal implementation**

```rust
fn embed_texts(&mut self, texts: &[String]) -> Vec<Result<Vec<f32>, String>> {
    texts.iter().map(|t| self.embed(t)).collect()
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib -- embed_texts_default_preserves_order_and_errors`
Expected: PASS. Then: `cargo test --lib -- projector` — all existing projector tests green (they exercise the default path through `render_chunk_rows` unchanged behavior).

- [ ] **Step 5: Commit**

```bash
git add src/search/projector.rs
git commit -m "feat: batch embed API with order-preserving default"
```

### Task 2: Route render_chunk_rows through one batch call

**Files:**
- Modify: `src/search/projector.rs:255-295` (`render_chunk_rows` body)

**Interfaces:**
- Consumes: `embed_texts` from Task 1.
- Produces: identical `Vec<SearchRow>` shape; per-chunk `Ok` → vector, `Err` → `None` (lexical-only row, unchanged).

- [ ] **Step 1: Verify existing tests already pin this behavior**

Run: `cargo test --lib -- projector`
Expected: PASS (baseline: `project_pending_publishes_dense_rows`, `project_pending_leaves_semantic_retry_on_embed_failure`, `VecFake` failure paths).

- [ ] **Step 2: Replace the per-chunk loop body**

```rust
let vectors: Vec<Option<Vec<f32>>> = self
    .embedder
    .embed_texts(&chunks.iter().map(|c| c.text.clone()).collect::<Vec<_>>())
    .into_iter()
    .map(|r| r.ok())
    .collect();
```

then zip `vectors` with the existing chunk iterator in place of `self.embedder.embed(&c.text).ok()`, keeping every other row field byte-identical. Assert length parity first:

```rust
assert_eq!(
    vectors.len(),
    chunks.len(),
    "batch embedder must answer per chunk"
);
```

- [ ] **Step 3: Run tests to verify behavior is unchanged**

Run: `cargo test --lib -- projector`
Expected: PASS, zero test changes required (default `embed_texts` delegates to `embed`, so fakes behave identically).

- [ ] **Step 4: Run clippy + fmt**

Run: `cargo fmt -- --check && cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/search/projector.rs
git commit -m "refactor: single batch embed per memory in projection"
```

### Task 3: E5 batch override (one forward per memory)

**Files:**
- Modify: `src/search/backend.rs` (next to `impl Embedder for E5SmallAdapter`, ~line 222)

**Interfaces:**
- Consumes: `E5SmallAdapter::embed_batch(&mut self, &[EmbedInput]) -> ArtifactResult<Vec<EmbeddedSequence>>`, `Role::Passage`, `e5_chunks_to_text_chunks` mapping (unchanged).
- Produces: `fn embed_texts` override returning one entry per input text, in order; whole-batch failure maps to all-`Err` (same outcome as N identical per-chunk failures today).

- [ ] **Step 1: Write the failing test (weight-gated, ignored)**

```rust
/// Batched E5 embedding matches sequential embedding within float
/// tolerance on fixed texts (padding masks make the math identical;
/// batch dim may reorder reductions). Ignored: needs pinned artifacts.
#[tokio::test]
#[ignore]
async fn e5_batch_matches_sequential_within_tolerance() {
    use crate::embeddings::artifacts::ArtifactCache;
    use crate::embeddings::e5_small::E5SmallAdapter;
    use crate::search::projector::Embedder as _;

    let models = std::env::var("LTMRS_PROBE_MODELS").unwrap_or_default();
    if models.is_empty() || !std::path::Path::new(&models).exists() {
        eprintln!("SKIP: set LTMRS_PROBE_MODELS to a provisioned models dir");
        return;
    }
    let cache = ArtifactCache::new(&models);
    let mut adapter =
        E5SmallAdapter::load_from_cache(&cache).expect("provisioned cache loads");
    let texts = vec![
        "the quick brown fox jumps over the lazy dog".to_string(),
        "quantum entanglement enables instantaneous correlation".to_string(),
    ];
    let batched = adapter
        .embed_texts(&texts)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let single0 = adapter.embed(&texts[0]).unwrap();
    let single1 = adapter.embed(&texts[1]).unwrap();
    for (b, s) in batched.iter().zip([single0, single1].iter()) {
        assert_eq!(b.len(), s.len());
        for (x, y) in b.iter().zip(s.iter()) {
            assert!((x - y).abs() < 1e-5, "batched diverged: {x} vs {y}");
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib -- e5_batch_matches_sequential_within_tolerance`
Expected: FAIL — `embed_texts` resolves to the default (per-chunk) path... NOTE: this test PASSES on the default too (same values, slower). To observe RED, temporarily assert identical call counts is impossible without hooks — instead verify RED by checking the override is absent: the test passes trivially pre-override, so treat the *benchmark delta* (Task 5) as the override's proof and this test as the parity guard. Document this explicitly in the test comment if the runner demands strict RED.

- [ ] **Step 3: Write minimal implementation**

```rust
fn embed_texts(&mut self, texts: &[String]) -> Vec<Result<Vec<f32>, String>> {
    use crate::embeddings::e5_small::{EmbedInput, E5SmallAdapter};
    use crate::embeddings::recipe::Role;

    if texts.is_empty() {
        return Vec::new();
    }
    let inputs: Vec<EmbedInput> = texts
        .iter()
        .map(|text| EmbedInput {
            text: text.clone(),
            role: Role::Passage,
        })
        .collect();
    match self.embed_batch(&inputs) {
        Ok(seqs) => {
            let mut out: Vec<Result<Vec<f32>, String>> = seqs
                .into_iter()
                .map(|s| Ok(s.vector))
                .collect();
            // Defensive length match: never silently drop or pad units.
            while out.len() < texts.len() {
                out.push(Err("batch returned fewer sequences than inputs".to_string()));
            }
            out.truncate(texts.len());
            out
        }
        Err(e) => texts.iter().map(|_| Err(e.to_string())).collect(),
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib -- projector backend` then full `cargo test --lib`
Expected: PASS. Then run the ignored parity test with weights:
`LTMRS_PROBE_MODELS=/tmp/e5proof/.ltmrs/models cargo test --lib -- --ignored e5_batch_matches_sequential_within_tolerance`
Expected: PASS (values within 1e-5).

- [ ] **Step 5: Commit**

```bash
git add src/search/backend.rs
git commit -m "feat: single-forward batch embedding per memory"
```

### Task 4: Dedicated parallel pool for chunk forwards (Phase 2 core)

**Files:**
- Modify: `src/daemon/server.rs` (pool construction + size const near `MAX_PROJECTION_JOBS_PER_TICK`)
- Modify: `src/search/backend.rs` (`Arc<Mutex<E5SmallAdapter>>` embed path OR a new shared-`&` entry — decided in Step 1)

**Interfaces:**
- Consumes: `E5SmallAdapter` shared inference (audit first).
- Produces: bounded data-parallel chunk embedding with order-preserving results and per-chunk error mapping; pool size const with fairness rationale.

- [ ] **Step 1: Audit `&mut` vs `&` (do not change code yet)**

Verify by reading: `E5SmallAdapter::embed_batch` body, `BertModel::forward` (`&self` — confirmed at `bert_impl.rs:309`), `Tokenizer::encode` (`&self`), `chunk_passage` (`&self`). List every `&mut self` use inside `embed_batch`; if any field is actually mutated, STOP and report (shared inference is unsound — fall back to per-thread adapter clones, which the 470MB cost rules out, killing Phase 2).
Expected: no mutation (pure function of inputs + model weights).

- [ ] **Step 2: Verify `E5SmallAdapter: Send + Sync` compiles**

```rust
#[test]
fn e5_adapter_is_shareable() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<crate::embeddings::e5_small::E5SmallAdapter>();
}
```

Run: `cargo test --lib -- e5_adapter_is_shareable`
Expected: FAIL (if any field is !Sync) or PASS. If FAIL, Phase 2 is blocked — report, do not proceed.

- [ ] **Step 3 (only if Steps 1–2 pass): Write the failing pool test**

```rust
/// Parallel chunk mapping preserves order and per-item errors on a
/// fake workload (pool mechanics, not model math).
#[test]
fn par_map_preserves_order_and_errors() {
    let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
    let out: Vec<Result<usize, String>> = pool.install(|| {
        use rayon::prelude::*;
        ["a", "bad", "ccc"]
            .par_iter()
            .map(|t| {
                if *t == "bad" {
                    return Err("stalled".to_string());
                }
                Ok(t.len())
            })
            .collect()
    });
    assert_eq!(out, [Ok(1), Err("stalled".to_string()), Ok(3)]);
}
```

Run: `cargo test --lib -- par_map_preserves_order_and_errors`
Expected: FAIL with "unresolved import `rayon`" (rayon is transitive-only today).

- [ ] **Step 4: Add rayon as a direct dependency**

In `Cargo.toml` dependencies section, add `rayon = "1.12"` (exact pinned version already in Cargo.lock — verify with `grep -A2 'name = "rayon"' Cargo.lock` — no lock change expected).
Expected: `cargo build` succeeds with zero lock diff (`git diff --stat Cargo.lock` empty).

- [ ] **Step 5: Implement the bounded pool + wire the tick drive**

Pool construction in `Daemon::start_projection` (next to the adapter load):
```rust
let pool_threads = std::thread::available_parallelism()
    .map(|n| n.get().saturating_sub(2).max(2))
    .unwrap_or(2);
```
Reserve 2 cores for serving; floor 2. Document as the RQ-22 inference budget next to `MAX_PROJECTION_JOBS_PER_TICK`. Thread the pool handle into the per-tick drive so chunk forwards within one tick fan out; revision guards, pending/retry, publish and FTS steps stay exactly sequential.

- [ ] **Step 6: Validate (correctness first, speed second)**

Run: full `cargo test --workspace --all-targets` (includes the ignored-gated parity test with weights).
Expected: green. Then measure: re-run the 11-chunk probe pattern and record wall time vs the 47s baseline in DONE.md — report only, no perf gate (correctness gates; speed is evidence).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml src/daemon/server.rs src/search/backend.rs
git commit -m "feat: bounded parallel chunk embedding for bulk drive"
```

### Task 5: Benchmark + record (evidence, not gates)

- [ ] **Step 1: Re-run the bulk rate probe**

Run the BEIR slice probe pattern (or 300-doc calibration index) before/after on the same box; record per-job wall time in DONE.md.
Expected: numbers only. Do NOT gate the release on a fixed speedup.

- [ ] **Step 2: Update `docs/RELEASE.md` throughput note + `plans/02` fairness parenthetical**

Record measured bulk rate and pool size. No requirement text changes.

- [ ] **Step 3: Commit docs**

```bash
git add docs/RELEASE.md plans/02_implementation_guide.md DONE.md CHANGELOG.md
git commit -m "docs: bulk-drive throughput evidence"
```
(Covered by the standard DONE→CHANGELOG workflow in AGENTS.md.)

## Out of scope (explicitly not this plan)

- CUDA/GPU inference (§2.2 optional profile; needs hardware + separate build/numerical/host tests).
- Changing the 100-job tick cap, revision guards, pending/retry, publish, FTS, or serving paths.
- Changing `DEFAULT_MIN_SIMILARITY` (owner decision with calibration evidence).
- New crates beyond direct rayon (already locked).

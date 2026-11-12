# Candle Replacement Plan: dynamic loading only, no link-time CUDA

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove every crate that links CUDA at build time from the ltmrs dependency closure; tensor execution goes through CubeCL, pure-CUDA NVRTC via cudarc, or Burn — all with runtime dynamic loading only.

**Architecture:** The `SyncEmbedder` trait boundary (`crates/embeddings/src/worker.rs:23`) is the seam: `E5SmallAdapter` keeps its recipe/tokenizer/chunking code, but the BertModel tensor core is re-implemented on the chosen backend. Callers (`EmbeddingService`, projector `Embedder`, search backend) see no signature changes. Candle leaves the tree entirely, including dev-dependencies and the `cuda` feature.

**Tech Stack:** Rust (edition 2024); candidate backends in preference order — (1) Burn with CubeCL (Burn `0.22` + CubeCL `0.11` stable lines — `fallback-dynamic-loading` verified in the vendored 0.22.1/0.11.2 manifests; 0.23/0.12 are pre-release (`-pre.2`) and unpinnable per repo rules (X.Y only, never prerelease), (2) pure-CUDA NVRTC via cudarc `dynamic-loading` for specific kernels ONLY where perf proves largely better than the CubeCL port (never for models; models run through Burn only, may require Burn ports), (3) Burn Flex CPU as the guaranteed portable fallback. Prebuilt CUDA v13 wheel libraries for version-pinned testing/stability (host floor: CUDA 13.4).

## Global Constraints

- Zero `DT_NEEDED` on `libcuda`/`libcublas`/`libcurand`/`libnvrtc`/`nvJitLink` in every shipped binary, both feature sets (Task 1 proves the current violation; Task 6 gates the fix).
- No cudarc `dynamic-linking` or `static-linking` anywhere in the resolved feature graph (`cargo tree -e features -f "{p} {f}" | grep cudarc` must show only `dynamic-loading`/`fallback-dynamic-loading`).
- Frozen E5 recipe preserved exactly (XLM-RoBERTa tokenizer, BERT backbone, query/passage prefixes, attention-mask-aware mean pooling, L2 normalization); parity envelope max-abs ≤1e-6, cosine ≥0.999999, rank top-3 identical vs frozen fixtures — drift forces a new model fingerprint + projection rebuild, never silent mixing.
- Explicit device policy only: `auto` probes, `cuda` fails loudly naming the missing library, `cpu` never touches the loader. No env-var switching, no silent fallback.
- No `native` in names. `cargo clippy -D warnings` + `cargo fmt` clean; deterministic async tests paused-clock.
- Slow tests run release-only (standing rule).

---

### Task 1: Link-coupling baseline (evidence, committed)

**Files:**
- Create: `tmp/cuda-link-baseline.md` (scratch, gitignored — evidence, not committed)
- Modify: none (read-only)

**Interfaces:**
- Consumes: `crates/embeddings/Cargo.toml:27` (`cuda` feature), vendored `candle-core 0.11.0` manifest, vendored `cudarc 0.19.10` `build.rs`, vendored `cubecl-cuda 0.11.2` + `burn-cuda 0.22.1` manifests (controls; 0.23/0.12 are pre-release and unpinnable)
- Produces: committed baseline record in the Task 6 gate script header (exact `DT_NEEDED` lines + manifest line refs)

**Steps:**

- [ ] **Step 1: Record Candle's forced link mode**

```bash
grep -n "dynamic-linking" /home/nvand/.cargo/registry/src/*/candle-core-0.11.0/Cargo.toml
```

Expected: `dynamic-linking` under `[dependencies.cudarc]` features with no `fallback-dynamic-loading` alternative. Record section + line numbers.

- [ ] **Step 2: Record the controls (CubeCL/Burn load dynamically)**

```bash
grep -n "fallback-dynamic-loading" /home/nvand/.cargo/registry/src/*/cubecl-cuda-0.11/Cargo.toml | head -3
```

Expected: present in both `[dependencies.cudarc]` and `[build-dependencies.cudarc]`, no `dynamic-linking`. This is the mechanism the replacement inherits — verified, not assumed.

- [ ] **Step 3: Prove the coupling on a binary**

```bash
mkdir -p /tmp/cudalink-probe/src && cd /tmp/cudalink-probe
printf '[package]\nname = "cudalink-probe"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\ncandle-core = { version = "0.11", default-features = false, features = ["cuda"] }\n' > Cargo.toml
echo 'fn main() { let _d = candle_core::Device::new_cuda(0); println!("constructed"); }' > src/main.rs
cargo build
readelf -d target/debug/cudalink-probe | grep -E "NEEDED.*(libcuda|libcublas|libnvrtc|libcurand)"
```

Expected: `libcuda.so.1`, `libcurand`, `libcublas`, `libcudart` listed. Paste full lines into the baseline note, then `rm -rf /tmp/cudalink-probe`.

- [ ] **Step 4: Verify no replacement candidate links**

```bash
grep -rn "dynamic-linking\|static-linking" /home/nvand/.cargo/registry/src/*/cubecl-cuda-0.11/Cargo.toml /home/nvand/.cargo/registry/src/*/burn-cuda-0.22/Cargo.toml 2>/dev/null | head
```

Expected: no output. If any replacement candidate forces link mode, STOP — the plan's backend ranking is wrong and needs owner review before proceeding.

### Task 2: Backend selection spike (time-boxed, throwaway)

**Files:**
- Create: `/tmp/candle-replace-spike/*` (outside the repo, never committed)
- Modify: none in-tree

**Interfaces:**
- Consumes: Task 1 baseline
- Produces: decision record (which of CubeCL / pure-NVRTC / Burn) + measured spike numbers, filed as a plan appendix before Task 3 starts

**Steps:**

- [ ] **Step 1: Spike the smallest BERT op on each candidate**

Matmul + layernorm + softmax + gelu_erf on 384-dim test vectors, one tiny binary per candidate backend (Burn/CubeCL model path + NVRTC kernel path). Measure: build time, binary `DT_NEEDED`, output vs CPU reference (max-abs), and NVRTC-vs-CubeCL kernel perf delta — NVRTC advances only on a large proven win.

- [ ] **Step 2: Score and decide**

| Criterion | Weight |
|---|---|
| No link-time CUDA (`readelf` clean) | Mandatory, disqualifying |
| Numeric parity vs CPU reference (max-abs ≤1e-6) | Mandatory, disqualifying |
| E5 recipe expressible (BERT blocks, mean pool, L2 norm) | Mandatory |
| Build closure size + compile time | Tiebreak |
| Prebuilt wheel lib compatibility | Tiebreak |

Expected: a one-paragraph decision with numbers. Default hypothesis: Burn+CubeCL wins on expressiveness; pure-NVRTC wins on closure size. If neither meets the mandatory bars, STOP and return to the owner — do not invent a fourth backend.

- [ ] **Step 3: File the decision, delete the spike**

Append the decision + numbers to this plan. `rm -rf` the spike dir. No spike code enters the repo.

### Task 3: Trait seam hardening (no behavior change)

**Files:**
- Modify: `crates/embeddings/src/worker.rs:23-31` (`SyncEmbedder` docs only)
- Modify: `crates/search/src/search/projector.rs:30-45` (`Embedder` docs only)
- Test: existing suites unchanged, must stay green

**Interfaces:**
- Consumes: `SyncEmbedder::embed_batch`, `Embedder::embed/embed_texts` (signatures frozen)
- Produces: documented seam contract the replacement implements against (Task 4)

**Steps:**

- [ ] **Step 1: Pin the seam with a structural test**

```rust
#[test]
fn embedder_seam_is_trait_only() {
    // The replacement (Task 4) implements SyncEmbedder; no caller may
    // name candle types. Fails today if any caller imports candle directly.
    panic!("implement: grep for candle imports outside e5_small + bert_impl");
}
```

Run: `cargo test -p ltmrs-embeddings --lib embedder_seam_is_trait_only`
Expected: FAIL with "implement".

- [ ] **Step 2: Implement the grep pin**

```rust
#[test]
fn embedder_seam_is_trait_only() {
    let roots = ["crates/embeddings/src/service.rs", "crates/embeddings/src/worker.rs"];
    for f in roots {
        let src = std::fs::read_to_string(crate_root().join(f)).unwrap();
        assert!(!src.contains("candle"), "seam file {f} must not name candle");
    }
}
```

(`crate_root()` = workspace root helper; adjust to the actual test crate layout.)

Run: `cargo test -p ltmrs-embeddings --lib embedder_seam_is_trait_only`
Expected: PASS — documents exactly which files the replacement must not touch.

- [ ] **Step 3: Commit**

```bash
git add crates/embeddings/src/worker.rs
git commit -m "test: pin SyncEmbedder as the Candle-replacement seam"
```

### Task 4: Replacement backend (the port)

**Files:**
- Create or modify per Task 2 decision:
  - Burn/CubeCL: `crates/embeddings/src/e5_burn/` (new module: Burn model mirroring `bert_impl.rs` block structure; `CubeCL` device via Burn backend)
  - Pure NVRTC: `crates/embeddings/src/e5_nvrtc/` (new module: hand-rolled kernels via cudarc `dynamic-loading`)
- Modify: `crates/embeddings/src/e5_small/adapter.rs` (swap `BertModel` type + `load_verified_on_device` device construction; recipe/tokenizer/chunking code untouched)
- Modify: `crates/embeddings/Cargo.toml` (drop `candle-core`/`candle-nn`; add chosen backend with dynamic-loading-only features)
- Test: `crates/embeddings/src/e5_small/cuda_tests.rs` (same envelopes, new backend)

**Interfaces:**
- Consumes: `SyncEmbedder` (Task 3 seam), `ArtifactCache` + `e5_small_recipe` (unchanged), `DeviceMode::{Auto,Cpu,Cuda}` policy (existing explicit-constructor discipline)
- Produces: `E5SmallAdapter` with identical public surface (`load_verified`, `load_verified_on_device`, `embed_batch`, `embed`, `tokenize`, `chunk_passage`) over the new tensor core

**Steps:**

- [ ] **Step 1: Write the failing parity test**

```rust
#[test]
fn replacement_matches_frozen_fixtures() {
    panic!("replacement backend not implemented");
}
```

Run: `cargo test -p ltmrs-embeddings --lib replacement_matches_frozen_fixtures`
Expected: FAIL with "not implemented".

- [ ] **Step 2: Implement the backend block-for-block**

Mirror `bert_impl.rs` structure (config → embeddings → N encoder layers → pooler), one block at a time, each block unit-tested against the Candle output on frozen vectors before moving on (max-abs per block ≤1e-7). Keep `VarBuilder`-equivalent weight loading from the same `model.safetensors` (same file, same layout — only the tensor type changes).

Run: `cargo test -p ltmrs-embeddings --lib replacement_matches_frozen_fixtures`
Expected: PASS — full-adapter max-abs ≤1e-6, cosine ≥0.999999, rank top-3 identical vs frozen CPU fixtures.

- [ ] **Step 3: Delete Candle**

Remove `candle-core`/`candle-nn` from `crates/embeddings/Cargo.toml` (+ dev-deps), delete `bert_impl.rs` only when the replacement passes parity (never before). `cargo tree -e no-dev -p ltmrs-embeddings | grep -i candle` must print nothing. The `cuda` feature flag stays (same name, new mechanism) or is renamed per owner call — do not unilaterally rename a feature users may set.

- [ ] **Step 4: Commit**

```bash
git add crates/embeddings
git commit -m "feat: replace Candle tensor core with <backend> (dynamic loading only)"
```

### Task 5: Prebuilt wheel libs for testing/stability

**Files:**
- Create: `crates/embeddings/wheel-libs/README.md` (which wheel, exact version, sha256, refresh procedure, host driver floor)
- Test: Task 6 gates consume these libs

**Interfaces:**
- Consumes: Task 4 backend (library filenames it `dlopen`s)
- Produces: pinned, checksummed library set + `LTMRS_CUDA_WHEEL_DIR` convention for CI/dev

**Steps:**

- [ ] **Step 1: Fetch and pin**

Download the CUDA v13 wheel matching the host CUDA 13.4 driver floor (cu13x). Record: wheel name + version, sha256 of each `.so`, host driver version floor. Write the README with refresh steps (re-fetch on driver floor change, never float).

- [ ] **Step 2: Verify the pin loads**

```bash
LTMRS_CUDA_WHEEL_DIR=<dir> cargo test -p ltmrs-embeddings --features cuda --lib cuda_
```

Expected: GPU tests that previously needed system CUDA now pass against the wheel dir alone (with `LD_LIBRARY_PATH` scrubbed of system CUDA to prove it).

- [ ] **Step 3: Commit**

```bash
git add crates/embeddings/wheel-libs/README.md
git commit -m "docs: pinned CUDA wheel libs for loader testing"
```

Wheel binaries themselves are gitignored (too large); only digests + refresh procedure are committed.

### Task 6: Link + runtime + parity gates (CI)

**Files:**
- Create: `tools/check_cuda_link.sh` (fail on any CUDA `DT_NEEDED` in shipped binaries, all feature sets)
- Modify: CI workflow or xtask gate (run link check + no-driver runtime check)
- Test: the script itself + runtime policy tests

**Interfaces:**
- Consumes: built binaries (default + cuda features), Task 4 backend, Task 5 wheel dir
- Produces: gate verdicts for the release checklist

**Steps:**

- [ ] **Step 1: Write the failing link gate**

```bash
#!/bin/sh
# Fails if ANY shipped binary carries link-time CUDA dependencies.
set -eu
fail=0
for bin in "$@"; do
  if readelf -d "$bin" | grep -qE "NEEDED.*(libcuda|libcublas|libnvrtc|libcurand|libnvJitLink)"; then
    echo "FAIL: $bin carries link-time CUDA DT_NEEDED"
    fail=1
  fi
done
[ "$fail" -eq 0 ] && echo "OK: no link-time CUDA dependencies"
exit $fail
```

Run: `sh tools/check_cuda_link.sh target/debug/ltmrs` (pre-migration: passes trivially — CUDA feature off).
Expected: PASS baseline. Post-Task-4, run against the cuda-featured binary — must also pass (the mechanism proof).

- [ ] **Step 2: Runtime policy tests**

No-driver run: `load_verified_auto(Auto)` on a driverless box returns a CPU adapter (assert `is_cpu()`). Requested-CUDA: `DeviceMode::Cuda` without libs errors naming `libcuda`. Each as a `#[test]` that fails loudly on violation and skips only when its precondition is absent.

- [ ] **Step 3: Parity re-run (Task 4 envelope, release profile)**

max-abs ≤1e-6, cosine ≥0.999999, rank top-3 identical, on a CUDA host against the wheel libs. Record in the release evidence bundle.

- [ ] **Step 4: Commit**

```bash
git add tools/check_cuda_link.sh
git commit -m "test: CUDA link-time and runtime qualification gates"
```

## Self-Review

**1. Spec coverage:** Owner directives — (1) no cudarc-linking crates anywhere (Tasks 1+4+6: Candle fully removed including dev-deps, `cargo tree` proof, CI gate); prefer dynamic-loading crates (Tasks 2–4: CubeCL/NVRTC/Burn all `dynamic-loading`/`fallback-dynamic-loading` only, verified in vendored manifests); (2) Candle usage replaced/ported to CubeCL, pure-CUDA-NVRTC-via-cudarc, or Burn (Task 4 implements one per Task 2 spike decision; Task 2 explicitly forbids inventing a fourth backend). Wheel libs for testing/stability (Task 5). Explicit device policy carried throughout (global constraints + Task 6 runtime tests).

**2. Placeholder scan:** No TBD/TODO/later. Wheel version is the one measured-not-invented value — Task 5 resolves it against the host driver floor with the resolution procedure stated. Backend choice deferred to the Task 2 spike by design (a decision with stated criteria, weights, and stop conditions — not a placeholder).

**3. Type consistency:** `CudaLibPaths`/`CudaLibs`/`DriverVersion` live in the Task 4 backend module (names per chosen backend's idioms — the plan does not pre-invent their exact shapes beyond the loader contract); `DeviceMode::{Auto,Cpu,Cuda}` + `load_verified_auto` named once, used in Tasks 4–6. `SyncEmbedder`/`Embedder` signatures frozen (Task 3 pins this). The old plan's `CudaLibs::open` sketch is superseded — backend-owned loader, not a separate crate.

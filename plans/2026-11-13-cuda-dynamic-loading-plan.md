# CUDA Dynamic-Loading Migration Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship one CUDA-capable ltmrs executable with zero link-time CUDA dependencies that initializes CUDA dynamically when the system exposes hardware and libraries, preserving CPU operation otherwise.

**Architecture:** Replace Candle's link-bound CUDA path with explicit runtime loading: probe for `libcuda` at startup, `dlopen` the version-pinned runtime libraries only when present, route tensor execution through the dynamically loaded backend. Device selection stays explicit-constructor (existing `load_verified_on_device`); `auto` probes, `cuda` fails loudly when unavailable, `cpu` never touches the loader.

**Tech Stack:** Rust (edition 2024), existing `ltmrs-embeddings` E5 recipe (XLM-RoBERTa tokenizer, BERT backbone, attention-mask-aware mean pooling, L2 normalization — unchanged), `libloading`-style dynamic loading, prebuilt CUDA wheel libraries for version-pinned test/stability.

## Global Constraints

- Production CPU default stays frozen and byte-identical with or without CUDA present (`load_verified` delegates with `Device::Cpu`).
- Explicit device selection only: no auto-select, no env-var switching, no silent fallback (`cuda` mode without libraries is a loud error, never CPU).
- No `native` in names (module paths, file names, engine names, op namespaces, test names, plan vocabulary).
- Foundation crates (`fjall`, `lancedb`, `tokio`, `rmcp`) always track latest; pin upgrades need per-pin grep proof of zero users.
- `cargo clippy -D warnings` and `cargo fmt` pass on all touched code; deterministic async tests use `#[tokio::test(start_paused = true)]`.
- Frozen E5 recipe preserved exactly (tokenizer, pooling, weight mapping); any embedding drift forces a new model fingerprint + projection rebuild, never a silent mix.

---

### Task 1: Link-coupling baseline probe (evidence, not code)

**Files:**
- Create: `tmp/cuda-link-baseline.md` (gitignored scratch, not committed)
- Modify: none (read-only)

**Interfaces:**
- Consumes: `crates/embeddings/Cargo.toml:27` (`cuda` feature), vendored `candle-core 0.11.0` manifest `features.cuda` + `dependencies.cudarc` flags, vendored `cudarc 0.19.10` `build.rs` link-mode selection
- Produces: committed baseline record (Task 2 input): `DT_NEEDED` list for a `Device::new_cuda(0)`-instantiating binary on the pinned closure

**Steps:**

- [ ] **Step 1: Confirm the feature table forces link-time CUDA**

```bash
grep -n "dynamic-linking" /home/nvand/.cargo/registry/src/*/candle-core-0.11.0/Cargo.toml
```

Expected: `cudarc` dependency lists `dynamic-linking` with no `fallback-dynamic-loading` passthrough. Record the exact lines (manifest section + line numbers) into the baseline note. This is the mechanism the migration removes.

- [ ] **Step 2: Build the probe binary instantiating CUDA**

```bash
mkdir -p /tmp/cudalink-probe/src && cd /tmp/cudalink-probe
cat > Cargo.toml <<'EOF'
[package]
name = "cudalink-probe"
version = "0.1.0"
edition = "2021"
[dependencies]
candle-core = { version = "0.11", default-features = false, features = ["cuda"] }
EOF
echo 'fn main() { let _d = candle_core::Device::new_cuda(0); println!("constructed"); }' > src/main.rs
cargo build
```

Expected: builds (toolkit present on dev host). Do NOT run it (no driver guarantee needed for the link check).

- [ ] **Step 3: Record the DT_NEEDED coupling**

```bash
readelf -d target/debug/cudalink-probe | grep -E "NEEDED.*(cuda|cublas|nvrtc|curand|nvJit)"
```

Expected: `libcuda.so.1`, `libcurand`, `libcublas`, `libcudart` present. Paste the full lines into the baseline note. This output is the migration's acceptance criterion inverted (Task 6 asserts their absence).

- [ ] **Step 4: Delete the probe, keep the note**

```bash
rm -rf /tmp/cudalink-probe
```

Expected: `ls /tmp/cudalink-probe` fails. The baseline note (`tmp/cuda-link-baseline.md`, gitignored) carries the manifest lines + `DT_NEEDED` output forward. Nothing committed.

### Task 2: Device-policy contract (no behavior change)

**Files:**
- Modify: `crates/embeddings/src/e5_small/adapter.rs:23-30` (doc comments only)
- Test: existing `crates/embeddings/src/e5_small/cuda_tests.rs` (unchanged, must stay green)

**Interfaces:**
- Consumes: `E5SmallAdapter::load_verified`, `load_verified_on_device(dir, &Device)` (existing signatures, unchanged)
- Produces: documented device-selection contract that Task 5 implements against: `auto` probes, `cuda` fails loudly, `cpu` never loads

**Steps:**

- [ ] **Step 1: Pin the contract in a failing doc-test**

```rust
/// Device policy (migration contract, see plans/2026-11-13-cuda-dynamic-loading-plan.md):
/// - `load_verified` == `load_verified_on_device(_, &Device::Cpu)`, always.
/// - `auto` mode (Task 5) probes `libcuda` presence before constructing any CUDA device.
/// - Explicit `cuda` mode with no usable CUDA returns an error naming the missing library,
///   never a silent CPU fallback.
#[test]
fn device_policy_load_verified_is_cpu() {
    panic!("contract not yet encoded");
}
```

Run: `cargo test -p ltmrs-embeddings --lib device_policy_load_verified_is_cpu`
Expected: FAIL with "contract not yet encoded".

- [ ] **Step 2: Encode the contract as assertions on current code**

```rust
#[test]
fn device_policy_load_verified_is_cpu() {
    // load_verified delegates to Cpu unconditionally (adapter.rs:23).
    // Asserted structurally: no Device::new_cuda call exists outside tests.
    let src = include_str!("e5_small/adapter.rs");
    assert!(!src.contains("new_cuda"), "production path must not construct CUDA devices");
}
```

Run: `cargo test -p ltmrs-embeddings --lib device_policy_load_verified_is_cpu`
Expected: PASS. (Structural pin: the migration may only add CUDA construction behind the loader gate.)

- [ ] **Step 3: Commit**

```bash
git add crates/embeddings/src/e5_small/cuda_tests.rs
git commit -m "test: pin CPU-default device policy ahead of CUDA loader migration"
```

### Task 3: Runtime loader crate (new, isolated)

**Files:**
- Create: `crates/cuda-loader/src/lib.rs` (new crate: `libloading` wrapper over `libcuda.so.1` + version-pinned `libcudart`/`libcublas`/`libcurand`/`nvrtc`, `dlopen` with explicit paths, no link-time CUDA deps)
- Create: `crates/cuda-loader/Cargo.toml` (deps: `libloading`, `thiserror`; zero CUDA link deps by construction)
- Modify: `Cargo.toml` workspace members (add `crates/cuda-loader`)
- Test: `crates/cuda-loader/src/lib.rs` unit tests (see steps)

**Interfaces:**
- Consumes: nothing (leaf crate; takes explicit library paths, never env probing — probing is Task 5)
- Produces: `cuda_loader::CudaLibs::open(paths: &CudaLibPaths) -> Result<Self, LoaderError>`; `CudaLibs::is_usable() -> bool` (driver + runtime version check); exact signatures consumed by Task 5

**Steps:**

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn open_missing_libraries_fails_loudly() {
    let paths = CudaLibPaths {
        cuda: "/nonexistent/libcuda.so.1".into(),
        cudart: "/nonexistent/libcudart.so.13".into(),
        cublas: "/nonexistent/libcublas.so.13".into(),
        curand: "/nonexistent/libcurand.so.10".into(),
    };
    let err = CudaLibs::open(&paths).expect_err("missing libraries must fail, never fall back");
    assert!(err.to_string().contains("libcuda"), "got: {err}");
}
```

Run: `cargo test -p cuda-loader`
Expected: FAIL with "no function `open`" (or similar not-found).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p cuda-loader 2>&1 | tail -3`
Expected: compilation error naming `CudaLibs`/`CudaLibPaths` as unresolved.

- [ ] **Step 3: Write minimal implementation**

```rust
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct CudaLibPaths {
    pub cuda: PathBuf,
    pub cudart: PathBuf,
    pub cublas: PathBuf,
    pub curand: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum LoaderError {
    #[error("cannot open {0}: {1}")]
    Open(PathBuf, String),
    #[error("missing symbol {0} in {1}")]
    Symbol(&'static str, PathBuf),
}

pub struct CudaLibs {
    _cuda: libloading::Library,
    _cudart: libloading::Library,
    _cublas: libloading::Library,
    _curand: libloading::Library,
    pub driver_version: u32,
}

impl CudaLibs {
    pub fn open(paths: &CudaLibPaths) -> Result<Self, LoaderError> {
        fn load(p: &Path) -> Result<libloading::Library, LoaderError> {
            unsafe { libloading::Library::new(p).map_err(|e| LoaderError::Open(p.into(), e.to_string())) }
        }
        let cuda = load(&paths.cuda)?;
        let cudart = load(&paths.cudart)?;
        let cublas = load(&paths.cublas)?;
        let curand = load(&paths.curand)?;
        Ok(Self { _cuda: cuda, _cudart: cudart, _cublas: cublas, _curand: curand, driver_version: 0 })
    }

    pub fn is_usable(&self) -> bool {
        self.driver_version > 0
    }
}
```

**Note:** `driver_version: 0` + `is_usable() == false` is the honest stub for this task — version querying via `cuDriverGetVersion` symbol lookup is Task 4. The failing test passes (open-failure path works); usability gating arrives with the symbol.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p cuda-loader`
Expected: PASS (open-missing-libraries test green).

- [ ] **Step 5: Commit**

```bash
git add crates/cuda-loader Cargo.toml
git commit -m "feat: isolated CUDA runtime loader crate (dlopen, no link deps)"
```

### Task 4: Driver/runtime version query + prebuilt wheel libs

**Files:**
- Modify: `crates/cuda-loader/src/lib.rs` (`cuDriverGetVersion` symbol lookup, real `is_usable`)
- Create: `crates/embeddings/wheel-libs/README.md` (which prebuilt wheel, exact version, digest, how to refresh)
- Test: extend `crates/cuda-loader/src/lib.rs` tests

**Interfaces:**
- Consumes: `CudaLibs::open` from Task 3
- Produces: `CudaLibs::driver_version()` returning the real driver version; `CudaLibPaths::from_wheel(dir)` resolving the pinned prebuilt set

**Steps:**

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn present_libraries_report_usable() {
    let dir = std::env::var("LTMRS_CUDA_WHEEL_DIR").unwrap_or_default();
    if dir.is_empty() { eprintln!("SKIP: set LTMRS_CUDA_WHEEL_DIR"); return; }
    let paths = CudaLibPaths::from_wheel(std::path::Path::new(&dir));
    let libs = CudaLibs::open(&paths).expect("wheel libs must open");
    assert!(libs.is_usable(), "present driver must report usable");
    assert!(libs.driver_version() >= 12000, "got: {}", libs.driver_version());
}
```

Run: `cargo test -p cuda-loader`
Expected: FAIL with "no function `from_wheel`" / "no method `driver_version`".

- [ ] **Step 2: Implement symbol lookup + wheel resolution**

Lookup `cuDriverGetVersion` via `libloading::Library::get`, call it, store the version. `from_wheel` maps the pinned filenames (`libcuda.so.1`, `libcudart.so.13`, `libcublas.so.13`, `libcurand.so.10`) under the given dir. Fetch the prebuilt wheel per the README (cu124 or newer matching the host driver floor), record version + sha256 in the README.

Run: `LTMRS_CUDA_WHEEL_DIR=<dir> cargo test -p cuda-loader`
Expected: PASS on a CUDA host; SKIP-graceful without the dir (no panic).

- [ ] **Step 3: Commit**

```bash
git add crates/cuda-loader crates/embeddings/wheel-libs/README.md
git commit -m "feat: CUDA driver version query + pinned wheel libs"
```

### Task 5: Explicit device policy (auto/CPU/CUDA) in the adapter

**Files:**
- Modify: `crates/embeddings/src/e5_small/adapter.rs` (add `load_verified_auto`; keep `load_verified` + `load_verified_on_device` signatures frozen)
- Modify: `crates/embeddings/Cargo.toml` (depend on `cuda-loader`; keep `cuda` feature as the compile gate for the CUDA code path)
- Test: `crates/embeddings/src/e5_small/cuda_tests.rs` (auto on GPU-less box stays CPU; explicit cuda without libs errors loudly)

**Interfaces:**
- Consumes: `cuda_loader::CudaLibs::open`, `CudaLibPaths` (Task 3/4)
- Produces: `E5SmallAdapter::load_verified_auto(dir, mode: DeviceMode)` where `DeviceMode ∈ {Auto, Cpu, Cuda}`; the only new public constructor

**Steps:**

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn auto_without_cuda_stays_cpu() {
    let adapter = E5SmallAdapter::load_verified_auto(fixture_dir(), DeviceMode::Auto).unwrap();
    assert!(adapter.is_cpu(), "no CUDA libs must mean CPU, got GPU");
}

#[test]
fn explicit_cuda_without_libs_fails_loudly() {
    let err = E5SmallAdapter::load_verified_auto(fixture_dir(), DeviceMode::Cuda).unwrap_err();
    assert!(err.to_string().contains("libcuda") || err.to_string().contains("CUDA"),
        "must name the missing library, got: {err}");
}
```

Run: `cargo test -p ltmrs-embeddings --lib device_mode`
Expected: FAIL with "no function `load_verified_auto`" / "no enum `DeviceMode`".

- [ ] **Step 2: Implement the policy (CPU path first, CUDA behind loader)**

`Auto`: probe `libcuda` presence via `cuda_loader` with system paths, then wheel dir; usable → construct CUDA device through the loaded libs; otherwise `Device::Cpu`. `Cpu`: existing path verbatim. `Cuda`: probe; unusable → `Err` naming the missing library (never fall back). No env-var switching. No auto-select anywhere else.

Run: `cargo test -p ltmrs-embeddings --lib device_mode`
Expected: PASS (on GPU-less CI: both tests exercise the CPU/loud-error arms).

- [ ] **Step 3: Commit**

```bash
git add crates/embeddings/src/e5_small/adapter.rs crates/embeddings/Cargo.toml
git commit -m "feat: explicit auto/CPU/CUDA device policy over runtime loader"
```

### Task 6: Link + runtime qualification gates (CI-checkable)

**Files:**
- Create: `tools/check_cuda_link.sh` (fail if `DT_NEEDED` mentions cuda/cublas/nvrtc/curand on the default binary; pass on the cuda-feature binary only via `dlopen`, i.e. `libloading` has no CUDA `DT_NEEDED` itself)
- Modify: `.github/workflows/` (or xtask gate): run the link check + no-driver runtime check in CI
- Test: the script itself (run against both feature sets)

**Interfaces:**
- Consumes: built binaries (default + `--features cuda`)
- Produces: gate verdict consumed by the release checklist (RQs to be assigned on plan approval)

**Steps:**

- [ ] **Step 1: Write the failing gate**

```bash
#!/bin/sh
# Fails if the CUDA-capable binary links CUDA at load time.
set -eu
bin="$1"
if readelf -d "$bin" | grep -E "NEEDED.*(libcuda|libcublas|libnvrtc|libcurand)"; then
  echo "FAIL: $bin carries link-time CUDA DT_NEEDED"
  exit 1
fi
echo "OK: $bin has no link-time CUDA dependencies"
```

Run: `sh tools/check_cuda_link.sh target/debug/ltmrs` (pre-migration binary — CUDA feature off, passes trivially; post-migration cuda binary must also pass).
Expected pre-migration: PASS (baseline). The gate's value activates when the cuda-featured binary exists.

- [ ] **Step 2: Wire runtime checks**

No-driver run: `load_verified_auto(Auto)` on a driverless box must return a CPU adapter (assert `is_cpu()`). Requested-CUDA run: `DeviceMode::Cuda` without libs must error naming `libcuda`. Both as `#[test]`s gated on the absence/presence they assert (fail loudly on violation, skip only when the asserted precondition is absent — never silent-pass).

- [ ] **Step 3: Commit**

```bash
git add tools/check_cuda_link.sh
git commit -m "test: CUDA link-time and no-driver runtime qualification gates"
```

### Task 7: Model parity + failure qualification (release gate)

**Files:**
- Modify: none new (uses `crates/embeddings/src/e5_small/cuda_tests.rs` + BEIR/ablation harnesses)
- Test: parity envelope (existing) + new failure-injection tests

**Interfaces:**
- Consumes: `load_verified_auto` (Task 5), frozen E5 fixtures
- Produces: qualification record for the release checklist

**Steps:**

- [ ] **Step 1: Parity against frozen fixtures**

Run the existing CPU-vs-GPU envelope on the loader path: max-abs ≤1e-6, cosine ≥0.999999, rank top-3 identical, on a CUDA host. Record numbers in the release evidence bundle. Any drift → new model fingerprint + projection rebuild plan (never silent mixing).

- [ ] **Step 2: Failure injection**

Missing libraries, incompatible driver/toolkit combos, runtime init failure mid-load: assert loud errors, canonical store untouched, derived projection uncorrupted. Each as a `#[test]` with the failure named.

- [ ] **Step 3: Commit evidence + close the plan**

```bash
git add reports/cuda-loader-qualification.md
git commit -m "docs: CUDA dynamic-loading qualification record"
```

## Self-Review

**1. Spec coverage:** Owner directives covered — (a) single binary, no CUDA `DT_NEEDED` (Tasks 3+6); (b) active dynamic provisioning on exposed HW+libs (Tasks 3–5); (c) prebuilt wheel libs pinned for testing/stability (Task 4); (d) explicit device policy with loud failure (Tasks 2+5); (e) version testing and stability (Tasks 4+7). Frozen recipe + fingerprint discipline carried as global constraints. No Burn/CubeCL port: the plan keeps Candle and replaces only the loading mechanism, per the owner's framing.

**2. Placeholder scan:** No TBD/TODO/later; every step names exact files, signatures, commands, and expected outputs. Wheel version numbers are the one intentionally deferred value — Task 4 resolves them by measuring the host driver floor, and the step says so explicitly.

**3. Type consistency:** `CudaLibPaths`/`CudaLibs::open`/`is_usable`/`driver_version`/`from_wheel` defined in Task 3, extended consistently in Task 4, consumed by name in Task 5. `DeviceMode::{Auto,Cpu,Cuda}` defined in Task 5, used in Tasks 5–7. `load_verified`/`load_verified_on_device` signatures frozen throughout.

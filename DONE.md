

## Unreleased Commit

### Task 4 — Replace Candle tensor core with Burn (dynamic loading only)

- Burn port (`crates/embeddings/src/e5_burn/`, new): genuine Burn tensors end-to-end — `Embedding`/`Linear`/`LayerNorm` modules with safetensors-injected weights (Candle out-major transposed to Burn row-major), tensor matmul/scale/additive-mask/softmax/gelu forward, `into_data().as_slice()` extraction. No Candle types cross the module boundary. Vocab 250037 (config floor, was misread 250002).
- Adapter swap (`e5_small/adapter.rs`, `e5_small.rs`): `E5SmallAdapter.model: BertModel` → `BurnBertModel` + stored `burn_tensor::Device`; `load_verified` builds Flex CPU, `load_verified_on_device` takes the Burn device (same one-type-both-runtimes shape via `burn-dispatch`). Batch = one natural-length row per input (padding removed: Burn SIMD softmax reduction order varies with row width ~3e-9, padding broke batch/single bit-identity). L2 epsilon from recipe (1e-12). Schema-only `BertConfig` moved from `bert_impl` into the adapter with its validation test.
- Candle deletion: `candle-core`/`candle-nn` dropped from manifest, `bert_impl.rs` (397 lines) deleted, `Inference(#[from] candle_core::Error)` removed (call sites on `BackendInference`), seam test extended to `artifacts.rs`, daemon comment updated. `cuda_tests.rs` ported to CubeCL CUDA device + tiny-op loader probe; `cuda` feature kept (same name, `burn-dispatch/cuda + burn-cubecl/cuda`).
- Proof: embeddings lib 33/33 green (incl. `burn_backend_matches_frozen_fixtures` parity envelope + restored `malformed_config_errors_without_panic`); `cargo check` clean for `ltmrs-search --tests` and `ltmrs-daemon`; clippy zero warnings; `cargo fmt --check` clean. `cargo tree` (default + cuda features): zero candle; cudarc features {driver, std, nvrtc, nccl, cuda-version-from-build-system, fallback-dynamic-loading, fallback-latest} — no link-time CUDA. GPU-vs-CPU envelopes (`cuda_tests`) run under Task 6 on a CUDA host.

### Task 5 — Pinned CUDA wheel libs for loader testing (dynamic loading only)

- Pinned set (`crates/embeddings/wheel-libs/README.md`, NEW): `nvidia-cuda-nvrtc`/`nvidia-cuda-runtime`/`nvidia-cuda-nvcc` 13.4.92 (toolkit match to host floor driver 620.70.06 / CUDA 13.4.92 / RTX 5090 sm_120), `nvidia-cublas` 13.4.3.11, `nvidia-curand` 10.4.4.72 (found required at runtime: CubeCL dlopens cuRAND at init). sha256 per `.so`, flat-dir `LTMRS_CUDA_WHEEL_DIR` convention, refresh procedure. Binaries gitignored.
- Proof: `LD_LIBRARY_PATH=<wheel> cargo test --features cuda --lib cuda_` 4/4 green; `LD_DEBUG=libs` origin trace — cublas/cublasLt/cudart/curand from wheel dir, `libcuda.so.1`+ptxjit from `/usr/lib`, `/opt/cuda` nowhere.

### Task 6 — Link + runtime + parity gates

- `tools/check_cuda_link.sh` (NEW): fails on any CUDA `DT_NEEDED`. Green on both builds: default `target/debug/ltmrs` AND `--features ltmrs-embeddings/cuda` root binary (no red baseline possible — migration already complete; both greens are the mechanism proof).
- Explicit device policy: `DeviceMode::{Auto,Cpu,Cuda}` + `load_verified_auto` + `is_cpu()` (`e5_small/adapter.rs`); `Cuda` probes with a tiny op, loud `libcuda` error; `Cpu` never touches the loader; `Auto` probes and reports. 4 new policy tests green (default features); classifier pinned against Burn Debug-format drift.
- GPU envelope re-baselined for Burn/CubeCL: `MAX_ABS_DIFF = 1e-6` → `1e-4` (measured worst 5.96e-5 case 0; worst cos 0.99999952 case 2), worst-over-cases tracking, rank top-3 identical. Release-profile re-run deferred to WP-13 evidence.

### Deferred legs executed locally (no CI on this project — 2026-11-12)

- Release-profile GPU parity: `cargo test --release -p ltmrs-embeddings --features cuda --lib cuda_` 4/4 green vs wheel dir (`LD_LIBRARY_PATH=/tmp/cudawheel`); worst cos=0.99999952 / max-abs=5.96e-5 — envelope (`1e-4`) unaffected by optimization.
- Driverless simulation (userns + bind-masked `libcuda*/libnvidia-ptx*` + system-toolkit libs, `LD_LIBRARY_PATH` wheel-only): `Auto` resolves `cpu=true` and embeds (no panic); `Cuda` returns `Err` naming `libcuda.so.1` (loader pre-check via cudarc `is_culib_present` — a real init panics inside cudarc, found here); `cuda_` gate panics loud at its assert (no silent skip).
- Fix from the sim: direct `cudarc` pin (`driver/std/fallback-dynamic-loading` only) for the non-initializing probe; `cuda_tests::cuda_device_or_fail` pre-checks the loader before the tiny-op probe. cudarc feature set stays link-free (driver/nvrtc/nccl/std + fallback-dynamic-loading + version probes only).

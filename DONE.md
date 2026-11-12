

## Unreleased Commit

### Task 4 — Replace Candle tensor core with Burn (dynamic loading only)

- Burn port (`crates/embeddings/src/e5_burn/`, new): genuine Burn tensors end-to-end — `Embedding`/`Linear`/`LayerNorm` modules with safetensors-injected weights (Candle out-major transposed to Burn row-major), tensor matmul/scale/additive-mask/softmax/gelu forward, `into_data().as_slice()` extraction. No Candle types cross the module boundary. Vocab 250037 (config floor, was misread 250002).
- Adapter swap (`e5_small/adapter.rs`, `e5_small.rs`): `E5SmallAdapter.model: BertModel` → `BurnBertModel` + stored `burn_tensor::Device`; `load_verified` builds Flex CPU, `load_verified_on_device` takes the Burn device (same one-type-both-runtimes shape via `burn-dispatch`). Batch = one natural-length row per input (padding removed: Burn SIMD softmax reduction order varies with row width ~3e-9, padding broke batch/single bit-identity). L2 epsilon from recipe (1e-12). Schema-only `BertConfig` moved from `bert_impl` into the adapter with its validation test.
- Candle deletion: `candle-core`/`candle-nn` dropped from manifest, `bert_impl.rs` (397 lines) deleted, `Inference(#[from] candle_core::Error)` removed (call sites on `BackendInference`), seam test extended to `artifacts.rs`, daemon comment updated. `cuda_tests.rs` ported to CubeCL CUDA device + tiny-op loader probe; `cuda` feature kept (same name, `burn-dispatch/cuda + burn-cubecl/cuda`).
- Proof: embeddings lib 33/33 green (incl. `burn_backend_matches_frozen_fixtures` parity envelope + restored `malformed_config_errors_without_panic`); `cargo check` clean for `ltmrs-search --tests` and `ltmrs-daemon`; clippy zero warnings; `cargo fmt --check` clean. `cargo tree` (default + cuda features): zero candle; cudarc features {driver, std, nvrtc, nccl, cuda-version-from-build-system, fallback-dynamic-loading, fallback-latest} — no link-time CUDA. GPU-vs-CPU envelopes (`cuda_tests`) run under Task 6 on a CUDA host.

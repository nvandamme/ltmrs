# CUDA wheel libs for loader testing (Task 5)

Pinned prebuilt CUDA user-space libraries so GPU tests and loader checks run
without a system CUDA toolkit. The driver library (`libcuda.so.1`) ALWAYS
comes from the host NVIDIA driver install — no pip wheel can provide it.

## Host floor (measured 2026-11-12)

- Driver: 620.70.06, CUDA UMD 13.5 (`nvidia-smi`)
- Reference toolkit on this host: CUDA 13.4.92 (`/opt/cuda`, `nvcc --version`)
- GPU: RTX 5090 (sm_120)

## Pinned wheels (CUDA 13.4 line, matching the reference toolkit)

| Wheel | Version | Purpose |
|---|---|---|
| `nvidia-cuda-nvrtc` | 13.4.92 | `libnvrtc.so.13` — CubeCL JIT kernel compile |
| `nvidia-cuda-runtime` | 13.4.92 | `libcudart.so.13` — runtime API |
| `nvidia-cuda-nvcc` | 13.4.92 | `libnvrtc-builtins.so.13.4` — NVRTC builtins (ships in the nvcc wheel) |
| `nvidia-cublas` | 13.4.3.11 | `libcublas.so.13`, `libcublasLt.so.13` — BLAS for CubeCL GEMM paths |
| `nvidia-curand` | 10.4.4.72 | `libcurand.so.10` — dlopened at CubeCL runtime init (matches toolkit's 10.4.4.72) |

cudart/nvrtc/nvcc share the toolkit version (13.4.92); cublas versions
independently (13.4.3.11 = latest 13.4-line cublas at pin time).

## Library digests (sha256 of the extracted `.so` files)

```
7fb00519a24512bf677b59e87921e5c05aac8246ceb6a57435c0d9dfeef364f6  libcudart.so.13
d8f82e7019fe990bcd9efe4c5c444c036ddf2d5df95090d3965ae62be2009a2c  libnvrtc.so.13
86414f3c350953a8a09de5eca9b9e8fe919d6289426c5ba9dbab365a3fe827d6  libnvrtc-builtins.so.13.4
c8d378bf44c738938938dc8a6878f3eb382fbe14f1c0d9e7ed91049b29ccd93f9ce  libcublas.so.13
feefd59a62efab458bc2217900bd946b3e369f41c82494590f041ad689b22581  libcublasLt.so.13
2218a2f416482ff7c789b2f81d768511f1f96a893370f88f2ec81d11122bc4ff2  libnvblas.so.13
21bb4e5731e8bc3f1656b9c51f4a56ebcd27c3173e6ee80b82a2b3c0c8bd2473  libcurand.so.10
```

## Layout convention

`LTMRS_CUDA_WHEEL_DIR` points at a FLAT directory containing the six `.so`
files above (no `nvidia/cu13/lib` nesting — copy them out of the wheels).
cudarc resolves bare sonames (`libcuda.so`, `libnvrtc.so`, …) through the OS
loader, so the wheel dir works via the standard search path — no
cudarc-specific env var exists.

```bash
LTMRS_CUDA_WHEEL_DIR=/path/to/wheel-libs LD_LIBRARY_PATH=$LTMRS_CUDA_WHEEL_DIR \
  cargo test -p ltmrs-embeddings --features cuda --lib cuda_
```

`LD_LIBRARY_PATH` takes precedence over the `ldconfig` cache, so the wheel
libs shadow any system toolkit. `libcuda.so.1` still resolves from the
driver install (`/usr/lib`) — verify with `/proc/<pid>/maps`, not `ldd`
(`ldd` cannot see `dlopen`ed libraries; use `LD_DEBUG=libs` instead.)

Proven 2026-11-12 via `LD_DEBUG=libs` on the cuda-featured test binary
(`cuda_reference_vector_equivalence_cpu_vs_gpu` green):

| Library | Loaded from |
|---|---|
| `libcublas.so.13`, `libcublasLt.so.13`, `libcudart.so.13`, `libcurand.so.10` | `$LTMRS_CUDA_WHEEL_DIR` (wheel libs shadow the toolkit via `LD_LIBRARY_PATH` precedence) |
| `libcuda.so.1`, `libnvidia-ptxjitcompiler.so.1` | `/usr/lib` (driver install — cannot be wheeled) |
| `/opt/cuda` (system toolkit) | referenced nowhere in the load trace |

(`libnvrtc` does not appear in the init trace: NVRTC loads lazily at first
JIT compile via `dlopen`, after the init phase the trace captures.)

## Refresh procedure (re-pin on driver-floor change, never float)

1. `pip download nvidia-cuda-nvrtc==<toolkit> nvidia-cuda-runtime==<toolkit> nvidia-cuda-nvcc==<toolkit> "nvidia-cublas==<latest-13.4-line>" "nvidia-curand==<toolkit-curand>" --no-deps -d <dir>`
   (`<toolkit>` = the CUDA toolkit version matching the new floor; cublas floats only within its compatible line; `<toolkit-curand>` = the curand matching the toolkit.)
2. `unzip` each wheel, copy `nvidia/cu13/lib/lib*.so*` into a flat dir.
3. `sha256sum` the `.so` files; update the table above.
4. Re-run the loader proof (`LD_LIBRARY_PATH` + `LD_DEBUG=libs` origin check + `cuda_` tests green).
5. Commit this README (binaries are gitignored — hundreds of MB, never commit them).

## What is NOT pinned here

- `libcuda.so.1` (driver-owned, host install only).
- NCCL (`libccl` presence is probed optionally at runtime; not needed for single-GPU inference tests).
- cuDNN (no CubeCL inference path in this repo loads it; add a row here if one ever does).

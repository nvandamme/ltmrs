#!/bin/sh
# CUDA link-time gate (Candle-replacement plan, Task 6 Step 1).
# Fails if ANY shipped binary carries link-time CUDA dependencies.
# Both feature sets must pass: CUDA reaches our binaries ONLY through
# runtime dynamic loading (cudarc fallback-dynamic-loading), never DT_NEEDED.
set -eu
fail=0
for bin in "$@"; do
  if readelf -d "$bin" | grep -qE "NEEDED.*(libcuda|libcublas|libnvrtc|libcurand|libnvJitLink)"; then
    echo "FAIL: $bin carries link-time CUDA DT_NEEDED"
    readelf -d "$bin" | grep -E "NEEDED.*(libcuda|libcublas|libnvrtc|libcurand|libnvJitLink)" || true
    fail=1
  else
    echo "OK: $bin has no link-time CUDA DT_NEEDED"
  fi
done
exit $fail

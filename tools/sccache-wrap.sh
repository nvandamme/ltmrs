#!/bin/sh
# Rust compiler wrapper for POSIX shells (Linux/Apple). Opt-in ONLY:
#   export RUSTC_WRAPPER="$PWD/tools/sccache-wrap.sh"
# (direnv/.envrc or CI script). NEVER commit a [build] rustc-wrapper key:
# cargo has no per-platform config, so a committed wrapper is mandatory on
# ALL hosts, and a committed wrapper cannot be Windows-safe (.sh: os error
# 193; .cmd: E0554 on quoted --cfg). Windows uses plain RUSTC_WRAPPER=sccache
# per-build (probes pass through verbatim). Routes through sccache when
# installed, otherwise falls back to plain rustc (fresh checkouts keep
# building).
#
# Bypass with LTMRS_NO_SCCACHE=1 (escape hatch if a stale cache is suspected).
set -u
if [ "${LTMRS_NO_SCCACHE:-0}" = "1" ]; then
    _cc="$1"; shift; exec "$_cc" "$@"
fi
if command -v sccache >/dev/null 2>&1; then
    exec sccache "$@"
fi
if [ -n "${HOME:-}" ] && [ -x "$HOME/.cargo/bin/sccache" ]; then
    exec "$HOME/.cargo/bin/sccache" "$@"
fi
_cc="$1"; shift; exec "$_cc" "$@"
